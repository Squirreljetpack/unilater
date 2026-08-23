use anyhow::{Context};
use serde::Serialize;
use std::process::Command;
use std::{env, fs, io};
use std::{collections::HashMap};
use std::path::{PathBuf, Component, Path};

pub fn write_files<P, T, E, S>(
    units: &HashMap<String, T>,
    output_dir: P,
    serializer: S,
) -> anyhow::Result<Vec<PathBuf>>
where
    P: AsRef<Path>,
    T: Serialize,
    S: Fn(&T) -> Result<String, E>,
    E: std::error::Error + Send + Sync + 'static,
{
    let output_dir = output_dir.as_ref();
    let mut written_files = Vec::new();
    for (filename, unit) in units {
    let string_content = serializer(unit)
        .map_err(|e| anyhow::Error::new(e).context(format!("Failed to serialize unit: {filename}")))?;


        let file_path = output_dir.join(filename);

        fs::write(&file_path, string_content)
            .with_context(|| format!("Failed to write to file: {file_path:?}"))?;
        written_files.push(file_path);
    }

    Ok(written_files)
}


pub fn print_files<T, E, S>(
    units: &HashMap<String, T>,
    serializer: S,
) -> anyhow::Result<()>
where
    T: Serialize,
    S: Fn(&T) -> Result<String, E>,
    E: std::error::Error + Send + Sync + 'static,
{
    let len = units.len();
    for (i, (filename, unit)) in units.iter().enumerate() {
        let string_content = serializer(unit)
            .map_err(|e| anyhow::Error::new(e).context(format!("Failed to serialize unit: {filename}")))?;

        println!("# {filename}\n{string_content}");
        if i + 1 < len {
            println!("\n---\n");
        }
    }
    Ok(())
}

use std::os::unix::fs::PermissionsExt;
fn has_permission(path: &PathBuf, bitflag: u32) -> bool {
    std::fs::File::open(path)
        .and_then(|file| file.metadata())
        .map(|metadata| metadata.permissions().mode() & bitflag != 0)
        .unwrap_or(false)
}


// todo: bitflag may not be best design
pub fn is_interactive() -> bool {
    has_permission(&PathBuf::from("/dev/tty"), 0o222)
}

extern "C" {
    fn geteuid() -> u32;
}

pub fn is_root() -> bool{
    unsafe { geteuid() == 0 }
}

pub fn systemctl_cmd(is_root: bool) -> Command {
    let mut cmd = Command::new("systemctl");
    if !is_root {
        cmd.arg("--user");
    }
    cmd
}

#[cfg(test)]
pub fn ask_confirm(_prompt: &str, yes_default: bool) -> io::Result<bool> {
    Ok(yes_default)
}

#[cfg(not(test))]
use demand::Confirm;

#[cfg(not(test))]
pub fn ask_confirm(prompt: &str, yes_default: bool) -> io::Result<bool> {
    if let Ok(auto) = std::env::var("SLATER_AUTO") {
        if auto.eq_ignore_ascii_case("true") || auto == "1" {
            return Ok(true);
        } else if auto.eq_ignore_ascii_case("false") || auto == "0" {
            return Ok(false);
        }
    }
    if ! is_interactive() {
        return Ok(yes_default);
    }

    if yes_default {
        Confirm::new(prompt)
            .affirmative("Yes")
            .negative("No")
            .run()
    } else {
        Confirm::new(prompt)
            .affirmative("No")
            .negative("Yes")
            .run()
            .map(|v| !v)
    }
}

pub fn normalize_path<P: AsRef<Path>>(path_input: P) -> String {
    normalize_path_with_base(None, path_input)
}

pub fn normalize_path_with_base<P: AsRef<Path>>(base: Option<&Path>, path_input: P) -> String {
    let path = path_input.as_ref();
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else if let Some(base_dir) = base {
        let base_abs = if base_dir.is_absolute() {
            base_dir.to_path_buf()
        } else {
            std::env::current_dir().unwrap_or_default().join(base_dir)
        };
        base_abs.join(path)
    } else {
        std::env::current_dir().unwrap_or_default().join(path)
    };

    let components = path.components().peekable();

    let mut ret = std::path::PathBuf::new();

    for component in components {
        match component {
            Component::Prefix(..) => ret.push(component.as_os_str()),
            Component::RootDir => ret.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                ret.pop();
            }
            Component::Normal(c) => ret.push(c),
        }
    }

    ret.to_string_lossy().to_string()
}


// windows not supported anyways
pub fn which(cmd: &str) -> Option<PathBuf> {
    if cmd.contains(std::path::MAIN_SEPARATOR) {
        let p = PathBuf::from(cmd);
        if p.is_file() && has_permission(&p, 0o111) {
            return Some(p);
        }
        return None;
    }

    if let Ok(paths) = env::var("PATH") {
        for path in env::split_paths(&paths) {
            let p = path.join(cmd);
            if p.is_file() && has_permission(&p, 0o111) {
                return Some(p);
            }
        }
    }
    None
}

// #[cfg(test)]
pub fn enter_test_dir() -> std::path::PathBuf {
    let dir = std::path::Path::new("/tmp/slater");
    std::fs::create_dir_all(dir).unwrap();
    std::env::set_current_dir(dir).unwrap();
    dir.to_path_buf()
}


#[cfg(test)]
  mod tests {
      use super::*;


      #[test]
      fn test_normalize_path() {
          let current_dir = std::env::current_dir().unwrap();
          let parent_dir = current_dir.parent().unwrap().to_str().unwrap();


          assert_eq!(normalize_path("/a/b/c"), "/a/b/c");
          assert_eq!(normalize_path("/a/b/../c"), "/a/c");
          assert_eq!(normalize_path("a/b/c"), format!("{}/a/b/c", current_dir.to_str().unwrap()));
          assert_eq!(normalize_path("a/../b/c"), format!("{}/b/c", current_dir.to_str().unwrap()));
          assert_eq!(normalize_path("../a/b/c"), format!("{parent_dir}/a/b/c"));
      }
  }