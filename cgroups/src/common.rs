use std::{
    fmt::{Debug, Display},
    fs::{self, File},
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
};

use anyhow::{bail, Context, Result};
use nix::{
    sys::statfs::{statfs, CGROUP2_SUPER_MAGIC, TMPFS_MAGIC},
    unistd::Pid,
};
use oci_spec::{FreezerState, LinuxDevice, LinuxDeviceCgroup, LinuxDeviceType, LinuxResources};
#[cfg(feature = "systemd_cgroups")]
use systemd::daemon::booted;
#[cfg(not(feature = "systemd_cgroups"))]
fn booted() -> Result<bool> {
    bail!("This build does not include the systemd cgroups feature")
}

use super::v1;
use super::v2;

use super::stats::Stats;

pub const CGROUP_PROCS: &str = "cgroup.procs";
pub const DEFAULT_CGROUP_ROOT: &str = "/sys/fs/cgroup";

pub trait CgroupManager {
    /// Adds a task specified by its pid to the cgroup
    fn add_task(&self, pid: Pid) -> Result<()>;
    /// Applies resource restrictions to the cgroup
    fn apply(&self, linux_resources: &LinuxResources) -> Result<()>;
    /// Removes the cgroup
    fn remove(&self) -> Result<()>;
    // Sets the freezer cgroup to the specified state
    fn freeze(&self, state: FreezerState) -> Result<()>;
    /// Retrieve statistics for the cgroup
    fn stats(&self) -> Result<Stats>;
    // Gets the PIDs inside the cgroup
    fn get_all_pids(&self) -> Result<Vec<Pid>>;
}

#[derive(Debug)]
pub enum CgroupSetup {
    Hybrid,
    Legacy,
    Unified,
}

impl Display for CgroupSetup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let print = match self {
            CgroupSetup::Hybrid => "hybrid",
            CgroupSetup::Legacy => "legacy",
            CgroupSetup::Unified => "unified",
        };

        write!(f, "{}", print)
    }
}

#[inline]
pub fn write_cgroup_file_str<P: AsRef<Path>>(path: P, data: &str) -> Result<()> {
    fs::OpenOptions::new()
        .create(false)
        .write(true)
        .truncate(false)
        .open(path.as_ref())
        .with_context(|| format!("failed to open {:?}", path.as_ref()))?
        .write_all(data.as_bytes())
        .with_context(|| format!("failed to write to {:?}", path.as_ref()))?;

    Ok(())
}

#[inline]
pub fn write_cgroup_file<P: AsRef<Path>, T: ToString>(path: P, data: T) -> Result<()> {
    fs::OpenOptions::new()
        .create(false)
        .write(true)
        .truncate(false)
        .open(path.as_ref())
        .with_context(|| format!("failed to open {:?}", path.as_ref()))?
        .write_all(data.to_string().as_bytes())
        .with_context(|| format!("failed to write to {:?}", path.as_ref()))?;

    Ok(())
}

#[inline]
pub fn read_cgroup_file<P: AsRef<Path>>(path: P) -> Result<String> {
    let path = path.as_ref();
    fs::read_to_string(path).with_context(|| format!("failed to open {:?}", path))
}

/// Determines the cgroup setup of the system. Systems typically have one of
/// three setups:
/// - Unified: Pure cgroup v2 system.
/// - Legacy: Pure cgroup v1 system.
/// - Hybrid: Hybrid is basically a cgroup v1 system, except for
///   an additional unified hierarchy which doesn't have any
///   controllers attached. Resource control can purely be achieved
///   through the cgroup v1 hierarchy, not through the cgroup v2 hierarchy.
pub fn get_cgroup_setup() -> Result<CgroupSetup> {
    let default_root = Path::new(DEFAULT_CGROUP_ROOT);
    match default_root.exists() {
        true => {
            // If the filesystem is of type cgroup2, the system is in unified mode.
            // If the filesystem is tmpfs instead the system is either in legacy or
            // hybrid mode. If a cgroup2 filesystem has been mounted under the "unified"
            // folder we are in hybrid mode, otherwise we are in legacy mode.
            let stat = statfs(default_root).with_context(|| {
                format!(
                    "failed to stat default cgroup root {}",
                    &default_root.display()
                )
            })?;
            if stat.filesystem_type() == CGROUP2_SUPER_MAGIC {
                return Ok(CgroupSetup::Unified);
            }

            if stat.filesystem_type() == TMPFS_MAGIC {
                let unified = Path::new("/sys/fs/cgroup/unified");
                if Path::new(unified).exists() {
                    let stat = statfs(unified)
                        .with_context(|| format!("failed to stat {}", unified.display()))?;
                    if stat.filesystem_type() == CGROUP2_SUPER_MAGIC {
                        return Ok(CgroupSetup::Hybrid);
                    }
                }

                return Ok(CgroupSetup::Legacy);
            }
        }
        false => bail!("non default cgroup root not supported"),
    }

    bail!("failed to detect cgroup setup");
}

pub fn create_cgroup_manager<P: Into<PathBuf>>(
    cgroup_path: P,
    systemd_cgroup: bool,
) -> Result<Box<dyn CgroupManager>> {
    let cgroup_setup = get_cgroup_setup()?;

    match cgroup_setup {
        CgroupSetup::Legacy | CgroupSetup::Hybrid => {
            log::info!("cgroup manager V1 will be used");
            Ok(Box::new(v1::manager::Manager::new(cgroup_path.into())?))
        }
        CgroupSetup::Unified => {
            if systemd_cgroup {
                if !booted()? {
                    bail!("systemd cgroup flag passed, but systemd support for managing cgroups is not available");
                }
                log::info!("systemd cgroup manager will be used");
                return Ok(Box::new(v2::SystemDCGroupManager::new(
                    DEFAULT_CGROUP_ROOT.into(),
                    cgroup_path.into(),
                )?));
            }
            log::info!("cgroup manager V2 will be used");
            Ok(Box::new(v2::manager::Manager::new(
                DEFAULT_CGROUP_ROOT.into(),
                cgroup_path.into(),
            )?))
        }
    }
}

pub fn get_all_pids(path: &Path) -> Result<Vec<Pid>> {
    log::debug!("scan pids in folder: {:?}", path);
    let mut result = vec![];
    walk_dir(path, &mut |p| {
        let file_path = p.join(CGROUP_PROCS);
        if file_path.exists() {
            let file = File::open(file_path)?;
            for line in BufReader::new(file).lines().flatten() {
                result.push(Pid::from_raw(line.parse::<i32>()?))
            }
        }
        Ok(())
    })?;
    Ok(result)
}

fn walk_dir<F>(path: &Path, c: &mut F) -> Result<()>
where
    F: FnMut(&Path) -> Result<()>,
{
    c(path)?;
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let path = entry.path();

        if path.is_dir() {
            walk_dir(&path, c)?;
        }
    }
    Ok(())
}

pub(crate) trait PathBufExt {
    fn join_safely(&self, p: &Path) -> Result<PathBuf>;
}

impl PathBufExt for PathBuf {
    fn join_safely(&self, p: &Path) -> Result<PathBuf> {
        if !p.is_absolute() && !p.as_os_str().is_empty() {
            bail!(
                "cannot join {:?} because it is not the absolute path.",
                p.display()
            )
        }
        Ok(PathBuf::from(format!("{}{}", self.display(), p.display())))
    }
}

pub(crate) fn default_allow_devices() -> Vec<LinuxDeviceCgroup> {
    vec![
        LinuxDeviceCgroup {
            allow: true,
            typ: Some(LinuxDeviceType::C),
            major: None,
            minor: None,
            access: "m".to_string().into(),
        },
        LinuxDeviceCgroup {
            allow: true,
            typ: Some(LinuxDeviceType::B),
            major: None,
            minor: None,
            access: "m".to_string().into(),
        },
        // /dev/console
        LinuxDeviceCgroup {
            allow: true,
            typ: Some(LinuxDeviceType::C),
            major: Some(5),
            minor: Some(1),
            access: "rwm".to_string().into(),
        },
        // /dev/pts
        LinuxDeviceCgroup {
            allow: true,
            typ: Some(LinuxDeviceType::C),
            major: Some(136),
            minor: None,
            access: "rwm".to_string().into(),
        },
        LinuxDeviceCgroup {
            allow: true,
            typ: Some(LinuxDeviceType::C),
            major: Some(5),
            minor: Some(2),
            access: "rwm".to_string().into(),
        },
        // tun/tap
        LinuxDeviceCgroup {
            allow: true,
            typ: Some(LinuxDeviceType::C),
            major: Some(10),
            minor: Some(200),
            access: "rwm".to_string().into(),
        },
    ]
}

pub(crate) fn default_devices() -> Vec<LinuxDevice> {
    vec![
        LinuxDevice {
            path: PathBuf::from("/dev/null"),
            typ: LinuxDeviceType::C,
            major: 1,
            minor: 3,
            file_mode: Some(0o066),
            uid: None,
            gid: None,
        },
        LinuxDevice {
            path: PathBuf::from("/dev/zero"),
            typ: LinuxDeviceType::C,
            major: 1,
            minor: 5,
            file_mode: Some(0o066),
            uid: None,
            gid: None,
        },
        LinuxDevice {
            path: PathBuf::from("/dev/full"),
            typ: LinuxDeviceType::C,
            major: 1,
            minor: 7,
            file_mode: Some(0o066),
            uid: None,
            gid: None,
        },
        LinuxDevice {
            path: PathBuf::from("/dev/tty"),
            typ: LinuxDeviceType::C,
            major: 5,
            minor: 0,
            file_mode: Some(0o066),
            uid: None,
            gid: None,
        },
        LinuxDevice {
            path: PathBuf::from("/dev/urandom"),
            typ: LinuxDeviceType::C,
            major: 1,
            minor: 9,
            file_mode: Some(0o066),
            uid: None,
            gid: None,
        },
        LinuxDevice {
            path: PathBuf::from("/dev/random"),
            typ: LinuxDeviceType::C,
            major: 1,
            minor: 8,
            file_mode: Some(0o066),
            uid: None,
            gid: None,
        },
    ]
}
