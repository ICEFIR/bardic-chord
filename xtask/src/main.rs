use anyhow::{bail, Context, Result};
use cargo_metadata::MetadataCommand;
use sha2::{Digest, Sha256};
use std::{
    env, fs,
    io::{self, Write},
    path::{Path, PathBuf},
    process::Command,
};
use tar::Builder as TarBuilder;
use xz2::write::XzEncoder;
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

const APP_NAME: &str = "bardic-chord";
const WINDOWS_TARGET: &str = "x86_64-pc-windows-gnu";

#[derive(Clone, Copy)]
enum ReleaseTarget {
    LinuxX64,
    WindowsX64Gnu,
}

impl ReleaseTarget {
    fn cli_name(self) -> &'static str {
        match self {
            Self::LinuxX64 => "linux",
            Self::WindowsX64Gnu => "windows",
        }
    }

    fn archive_name(self) -> &'static str {
        match self {
            Self::LinuxX64 => "bardic-chord-x86_64-unknown-linux-gnu.tar.xz",
            Self::WindowsX64Gnu => "bardic-chord-x86_64-pc-windows-gnu.zip",
        }
    }

    fn archive_root(self, version: &str) -> String {
        match self {
            Self::LinuxX64 => format!("{APP_NAME}-{version}-x86_64-unknown-linux-gnu"),
            Self::WindowsX64Gnu => format!("{APP_NAME}-{version}-x86_64-pc-windows-gnu"),
        }
    }

    fn binary_name(self) -> &'static str {
        match self {
            Self::LinuxX64 => APP_NAME,
            Self::WindowsX64Gnu => "bardic-chord.exe",
        }
    }

    fn binary_path(self, workspace_root: &Path) -> PathBuf {
        match self {
            Self::LinuxX64 => workspace_root.join("target/release/bardic-chord"),
            Self::WindowsX64Gnu => {
                workspace_root.join(format!("target/{WINDOWS_TARGET}/release/bardic-chord.exe"))
            }
        }
    }

    fn build(self, workspace_root: &Path) -> Result<()> {
        let mut command = Command::new("cargo");
        match self {
            Self::LinuxX64 => {
                command.args(["build", "-p", APP_NAME, "--release"]);
            }
            Self::WindowsX64Gnu => {
                command.args([
                    "zigbuild",
                    "-p",
                    APP_NAME,
                    "--release",
                    "--target",
                    WINDOWS_TARGET,
                ]);
            }
        }
        run_command(command, workspace_root)
    }
}

fn main() -> Result<()> {
    let workspace_root = workspace_root()?;
    let version = package_version(&workspace_root, APP_NAME)?;

    match parse_args()? {
        Task::Release { targets } => {
            let dist_dir = workspace_root.join("dist");
            fs::create_dir_all(&dist_dir)
                .with_context(|| format!("failed to create {}", dist_dir.display()))?;

            for target in targets {
                println!("==> building {}", target.cli_name());
                target.build(&workspace_root)?;

                println!("==> packaging {}", target.cli_name());
                let binary_path = target.binary_path(&workspace_root);
                ensure_exists(&binary_path)?;

                let archive_path = dist_dir.join(target.archive_name());
                if archive_path.exists() {
                    fs::remove_file(&archive_path).with_context(|| {
                        format!("failed to remove old archive {}", archive_path.display())
                    })?;
                }

                match target {
                    ReleaseTarget::LinuxX64 => {
                        package_tar_xz(
                            &archive_path,
                            &target.archive_root(&version),
                            &binary_path,
                            &workspace_root,
                            target.binary_name(),
                        )?;
                    }
                    ReleaseTarget::WindowsX64Gnu => {
                        package_zip(
                            &archive_path,
                            &target.archive_root(&version),
                            &binary_path,
                            &workspace_root,
                            target.binary_name(),
                        )?;
                    }
                }

                write_sha256(&archive_path)?;
                println!("   wrote {}", archive_path.display());
            }
        }
    }

    Ok(())
}

enum Task {
    Release { targets: Vec<ReleaseTarget> },
}

fn parse_args() -> Result<Task> {
    let mut args = env::args().skip(1);
    let Some(command) = args.next() else {
        return Ok(Task::Release {
            targets: vec![ReleaseTarget::LinuxX64, ReleaseTarget::WindowsX64Gnu],
        });
    };

    match command.as_str() {
        "release" => {
            let mut targets = Vec::new();
            while let Some(arg) = args.next() {
                match arg.as_str() {
                    "--target" => {
                        let Some(value) = args.next() else {
                            bail!("missing value after --target; use linux, windows, or all");
                        };
                        match value.as_str() {
                            "linux" => targets.push(ReleaseTarget::LinuxX64),
                            "windows" => targets.push(ReleaseTarget::WindowsX64Gnu),
                            "all" => {
                                targets.clear();
                                targets.extend([
                                    ReleaseTarget::LinuxX64,
                                    ReleaseTarget::WindowsX64Gnu,
                                ]);
                            }
                            _ => bail!("unsupported target `{value}`; use linux, windows, or all"),
                        }
                    }
                    other => bail!("unsupported argument `{other}`"),
                }
            }

            if targets.is_empty() {
                targets.extend([ReleaseTarget::LinuxX64, ReleaseTarget::WindowsX64Gnu]);
            }

            Ok(Task::Release { targets })
        }
        other => bail!("unknown xtask command `{other}`; supported: release"),
    }
}

fn workspace_root() -> Result<PathBuf> {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest_dir
        .parent()
        .map(Path::to_path_buf)
        .context("xtask is expected to live under the workspace root")
}

fn package_version(workspace_root: &Path, package_name: &str) -> Result<String> {
    let metadata = MetadataCommand::new()
        .manifest_path(workspace_root.join("Cargo.toml"))
        .no_deps()
        .exec()
        .context("failed to read Cargo workspace metadata")?;

    metadata
        .packages
        .into_iter()
        .find(|pkg| pkg.name == package_name)
        .map(|pkg| pkg.version.to_string())
        .with_context(|| format!("failed to find package version for `{package_name}`"))
}

fn run_command(mut command: Command, workspace_root: &Path) -> Result<()> {
    command.current_dir(workspace_root);
    let status = command.status().with_context(|| {
        let program = command.get_program().to_string_lossy().into_owned();
        format!("failed to start `{program}`")
    })?;

    if !status.success() {
        bail!("command exited with status {status}");
    }

    Ok(())
}

fn package_tar_xz(
    archive_path: &Path,
    archive_root: &str,
    binary_path: &Path,
    workspace_root: &Path,
    binary_name: &str,
) -> Result<()> {
    let archive_file = fs::File::create(archive_path)
        .with_context(|| format!("failed to create {}", archive_path.display()))?;
    let encoder = XzEncoder::new(archive_file, 9);
    let mut tar = TarBuilder::new(encoder);

    append_tar_path(
        &mut tar,
        binary_path,
        &PathBuf::from(archive_root).join(binary_name),
        true,
    )?;
    append_tar_path(
        &mut tar,
        &workspace_root.join("README.md"),
        &PathBuf::from(archive_root).join("README.md"),
        false,
    )?;
    append_tar_path(
        &mut tar,
        &workspace_root.join("LICENSE"),
        &PathBuf::from(archive_root).join("LICENSE"),
        false,
    )?;

    tar.finish().context("failed to finish tar archive")?;
    Ok(())
}

fn append_tar_path(
    tar: &mut TarBuilder<XzEncoder<fs::File>>,
    source: &Path,
    dest: &Path,
    executable: bool,
) -> Result<()> {
    ensure_exists(source)?;
    let mut header = tar::Header::new_gnu();
    let metadata = fs::metadata(source)
        .with_context(|| format!("failed to read metadata for {}", source.display()))?;
    header.set_metadata(&metadata);
    if executable {
        header.set_mode(0o755);
    } else {
        header.set_mode(0o644);
    }
    header.set_cksum();
    let mut file =
        fs::File::open(source).with_context(|| format!("failed to open {}", source.display()))?;
    tar.append_data(&mut header, dest, &mut file)
        .with_context(|| format!("failed to add {} to tar archive", dest.display()))?;
    Ok(())
}

fn package_zip(
    archive_path: &Path,
    archive_root: &str,
    binary_path: &Path,
    workspace_root: &Path,
    binary_name: &str,
) -> Result<()> {
    let archive_file = fs::File::create(archive_path)
        .with_context(|| format!("failed to create {}", archive_path.display()))?;
    let mut zip = ZipWriter::new(archive_file);

    append_zip_path(
        &mut zip,
        binary_path,
        &PathBuf::from(archive_root).join(binary_name),
        true,
    )?;
    append_zip_path(
        &mut zip,
        &workspace_root.join("README.md"),
        &PathBuf::from(archive_root).join("README.md"),
        false,
    )?;
    append_zip_path(
        &mut zip,
        &workspace_root.join("LICENSE"),
        &PathBuf::from(archive_root).join("LICENSE"),
        false,
    )?;

    zip.finish().context("failed to finish zip archive")?;
    Ok(())
}

fn append_zip_path(
    zip: &mut ZipWriter<fs::File>,
    source: &Path,
    dest: &Path,
    executable: bool,
) -> Result<()> {
    ensure_exists(source)?;
    let options = SimpleFileOptions::default()
        .compression_method(CompressionMethod::Deflated)
        .unix_permissions(if executable { 0o755 } else { 0o644 });

    zip.start_file(dest.to_string_lossy().as_ref(), options)
        .with_context(|| format!("failed to start {} in zip archive", dest.display()))?;
    let bytes = fs::read(source).with_context(|| format!("failed to read {}", source.display()))?;
    zip.write_all(&bytes)
        .with_context(|| format!("failed to write {} to zip archive", dest.display()))?;
    Ok(())
}

fn ensure_exists(path: &Path) -> Result<()> {
    if path.exists() {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("required file does not exist: {}", path.display()),
        ))
        .context("build output missing")
    }
}

fn write_sha256(path: &Path) -> Result<()> {
    let bytes = fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    let digest = hasher.finalize();
    let checksum = digest
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let checksum_path = PathBuf::from(format!("{}.sha256", path.display()));
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .context("artifact path is missing a file name")?;

    fs::write(&checksum_path, format!("{checksum}  {file_name}\n"))
        .with_context(|| format!("failed to write {}", checksum_path.display()))?;
    println!("   wrote {}", checksum_path.display());
    Ok(())
}
