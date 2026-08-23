use anyhow::{anyhow, Result, Context};
use log::{self, error, info};
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use serde_yaml::Value;
use std::{collections::HashMap, fs::File, io::{BufRead, BufReader}, path::{Path, PathBuf}, process::Command};

use crate::{utils::{ask_confirm, is_root, normalize_path, normalize_path_with_base, systemctl_cmd, which}, formats::{Ini, IniFiles, Section}};
use regex::Regex;



#[derive(Debug, Deserialize, Serialize)]
pub struct ComposeFile {
    pub services: HashMap<String, Value>,

    #[serde(flatten)]
    pub other: HashMap<String, Value>,
}




pub fn parse_qualified_name(output: &[u8]) -> Result<String> {
    let image_data: Result<JsonValue, _> = serde_json::from_slice(output);
    match image_data {
        Ok(image_data) => {
            if let Some(full_ref) = image_data
                .as_array()
                .and_then(|i| i.first())
                .and_then(|i| i.get("Ref"))
                .and_then(|i| i.as_str())
            {
                if let Some((name, _)) = full_ref.split_once('@') {
                    return Ok(name.to_string());
                } else {
                    log::warn!("Could not split image ref on '@': {full_ref}");
                }
            } else {
                log::warn!("Could not qualify name: 'Ref' field missing in manifest");
            }
        }
        Err(e) => {
            log::warn!("Could not parse JSON output from docker manifest: {e}");
        }
    }
    Err(anyhow!("Could not parse qualified image name"))
}


fn get_qualified_name(name: &str) -> Result<String> {
    // if std::env::var("SLATER_AUTO").map(|v| v == "true").unwrap_or(false) {
    //     return Ok(name.into())
    // }

    log::debug!("Attempting to qualify image name: {name}");

    match Command::new("docker")
        .arg("manifest")
        .arg("inspect")
        .arg("--verbose")
        .arg(name)
        .output()
    {
        Ok(output) => {
            if output.status.success() {
                if let Ok(qualified_name) = parse_qualified_name(&output.stdout) {
                    return Ok(qualified_name);
                }
            } else {
                log::warn!(
                    "docker manifest inspect for '{}' failed: {}",
                    name,
                    String::from_utf8_lossy(&output.stderr)
                );
            }
        }
        Err(e) => {
            log::error!("Failed to execute docker command: {e}");
        }
    }

    Err(anyhow!("Could not qualify image name: {}", name))
}

// podlet convert doesn't support ${} in places such as volumes so we offer to make replacements
fn replace_env_vars(value: &mut Value) -> Result<()> {
    match value {
        Value::String(s) => {
            let re = Regex::new(r"\$\{[a-zA-Z_][a-zA-Z_0-9]*\}")?;
            let mut new_s = s.to_string();
            let mut replacements_made = false;

            for cap in re.captures_iter(s) {
                let var = &cap[0];
                let var_name = &var[2..var.len() - 1];
                if let Ok(env_var) = std::env::var(var_name) {
                    if cfg!(feature = "integration-tests") || ask_confirm(
                        &format!(
                            "Replace '{var}' with '{env_var}'?"
                        ),
                        false,
                    )? {
                        new_s = new_s.replace(var, &env_var);
                        replacements_made = true;
                    }
                }
            }

            if replacements_made {
                *value = Value::String(new_s);
            }
        }
        Value::Mapping(map) => {
            for (_key, val) in map.iter_mut() {
                replace_env_vars(val)?;
            }
        }
        Value::Sequence(seq) => {
            for item in seq.iter_mut() {
                replace_env_vars(item)?;
            }
        }
        _ => {}
    }
    Ok(())
}

pub fn process_compose(mut file: ComposeFile, initial_dir: Option<&Path>) -> Result<ComposeFile> {
    if file.services.is_empty() {
        anyhow::bail!("No services found!");
    }

    let service_name = file.services.keys().next().cloned().unwrap();

    // insert required name field using first service
    // todo: prompt about name with default
    if !file.other.contains_key("name") {
        let initial_dirname = initial_dir
            .and_then(|p| p.file_name())
            .and_then(|s| s.to_str());
        if service_name != "app" {
            file.other
            .insert("name".to_string(), Value::String(service_name.clone()));
        } else {
             let default_name = initial_dirname.unwrap_or(&service_name);
            file.other
                .insert("name".to_string(), Value::String(default_name.to_string()));
        }
    }

    // Offer to rename the primary container to "app"
    if service_name != "app"
        && ask_confirm(
            &format!("Do you want to rename service '{service_name}' to 'app'?"),
            false,
        )? {
            if let Some(service) = file.services.remove(&service_name) {
                file.services.insert("app".to_string(), service);
            }
        }
    
    if let Some(dir) = initial_dir {
        let env_file = dir.join(".env");
        if env_file.exists() {
            info!("Sourcing env file for variable substitution");
            let file = File::open(env_file)?;
            let reader = BufReader::new(file);
            for line in reader.lines() {
                let line = line?;
                let trimmed = line.trim();
                if trimmed.is_empty() || trimmed.starts_with('#') {
                    continue;
                }
                if let Some((key, value)) = trimmed.split_once('=') {
                    let key = key.trim();
                    let mut value = value.trim();
                    if (value.starts_with('"') && value.ends_with('"'))
                        || (value.starts_with('\'') && value.ends_with('\''))
                    {
                        if value.len() >= 2 {
                            value = &value[1..value.len() - 1];
                        }
                    } else if let Some((val_part, _)) = value.split_once('#') {
                        value = val_part.trim();
                    }

                    if let Ok(existing_value) = std::env::var(key) {
                        if ! ask_confirm(
                            &format!(
                                "Environment variable '{key}' is already set to '{existing_value}'. Overwrite with '{value}' for variable substitution?"
                            ),
                            false,
                        )? {
                            continue;
                        }
                    }
                    std::env::set_var(key, value);
                }
            }
        }
    }

    for (_service_name, service) in file.services.iter_mut() {
        replace_env_vars(service)?;

        if let Some(service_map) = service.as_mapping_mut() {

            // Qualify image names
            if let Some(image_val) = service_map.get_mut(Value::String("image".to_string())) {
                if let Some(image) = image_val.as_str() {
                    if image.matches('/').count() < 2 {
                        if let Ok(image) = get_qualified_name(image) {
                            *image_val = image.into()
                        }
                    }
                }
            }

            // Canonicalize host and env paths
            if let Some(volumes_val) = service_map.get_mut(Value::String("volumes".to_string())) {
                if let Some(volumes) = volumes_val.as_sequence_mut() {
                    for volume in volumes.iter_mut() {
                        if let Some(volume_str) = volume.clone().as_str() {
                            let parts: Vec<&str> = volume_str.splitn(2, ':').collect();
                            if parts.len() == 2 {
                                let host_path = parts[0];
                                // Check not a named volume
                                if host_path.contains('/') || host_path.starts_with('.') {
                                    let new_volume = format!("{}:{}", normalize_path_with_base(initial_dir, host_path), parts[1]);
                                    *volume = Value::String(new_volume);
                                    log::debug!(
                                        "Volume path '{}' replaced with '{}'",
                                        volume_str,
                                        volume.as_str().unwrap()
                                    );
                                }
                            }
                        }
                    }
                }
            }

            if let Some(env_file_val) = service_map.get_mut(Value::String("env_file".to_string())) {
                match env_file_val.clone() {
                    Value::String(s) => {
                        if s.contains('/') || s.starts_with('.') {
                            let new_path = normalize_path_with_base(initial_dir, &s);
                            *env_file_val = Value::String(new_path);
                            log::debug!("env_file '{}' replaced with '{}'", s, env_file_val.as_str().unwrap());
                        }
                    }
                    Value::Sequence(mut seq) => {
                        for item in seq.iter_mut() {
                            if let Some(s) = item.clone().as_str() {
                                if s.contains('/') || s.starts_with('.') {
                                    let new_path = normalize_path_with_base(initial_dir, s);
                                    *item = Value::String(new_path.clone());
                                    log::debug!("env_file '{s}' replaced with '{new_path}'");
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }

        }
    }
    Ok(file)
}

pub const QUADLET_DELIMITER: &str = "\n---\n\n";

fn parse_raw_quadlets(output: &str) -> Result<IniFiles> {
    let mut units = IniFiles::new();
    for block in output.split(QUADLET_DELIMITER) {
        if let Some((first_line, rest)) = block.split_once('\n') {
            if let Some(stripped) = first_line.strip_prefix("# ") {
                let key = stripped.trim().to_string();
                let item: Ini = serde_ini::from_str(rest)?;
                units.insert(key, item);
            }
        } else {
            error!("Unexpected section of podlet output encountered, skipping");
        }
    }
    Ok(units)
}

pub fn get_raw_quadlets(filepath: &PathBuf) -> Result<IniFiles> {
    if which("podlet").is_none() {
        anyhow::bail!("podlet command not found. Please install podlet.");
    }

    let output = Command::new("podlet")
        .arg("compose")
        .arg("--pod")
        .arg(filepath)
        .output()?;

    if !output.status.success() {
        anyhow::bail!(
            "podlet conversion failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let output_str = String::from_utf8_lossy(&output.stdout);
    parse_raw_quadlets(&output_str)
}

pub fn process_quadlets(mut units: IniFiles, initial_dir: Option<&Path>) -> Result<IniFiles> {
    for (unit_name, unit_data) in units.0.iter_mut() {
        if unit_name.ends_with(".pod") {
            if ask_confirm(
                &format!("Add WantedBy=default.target to '{unit_name}'?"),
                true,
            )? {
                let install_section = unit_data.0.entry("Install".to_string()).or_insert_with(Section::new);
                install_section.insert("WantedBy".to_string(), "default.target".to_string());
            }
        } else if unit_name.ends_with(".container") {
            let unit_section = unit_data.0.entry("Unit".to_string()).or_insert_with(Section::new);
            if ask_confirm(
                &format!("Add After=local-fs.target network-online.target systemd-networkd-wait-online.service to '{unit_name}'?"),
                true,
            )? {
                unit_section.insert("After".to_string(), "local-fs.target network-online.target systemd-networkd-wait-online.service".to_string());
            }

            let service_section = unit_data.0.entry("Service".to_string()).or_insert_with(Section::new);
            // if .env exists in the same directory as .compose, include it into the systemd service
            if let Some(dir) = initial_dir {
                let env_file = dir.join(".env");
                let env_file_str=normalize_path(&env_file);
                if env_file.exists()
                    && ask_confirm(
                        &format!("Add EnvironmentFile={env_file_str} to '{unit_name}'?"),
                        true,
                    )? {
                        service_section.insert("EnvironmentFile".to_string(), env_file_str);
                    }
            }

            let container_section = unit_data.0.entry("Container".to_string()).or_insert_with(Section::new);

            let image_name = container_section.get("Image").map(|s| s.as_str()).unwrap_or("");
            let autoupdate_value = if image_name.contains('.') { "registry" } else { "local" };

            if ask_confirm(
                &format!("Add AutoUpdate={autoupdate_value} to '{unit_name}'?"),
                true,
            )? {
                container_section.insert("AutoUpdate".to_string(), autoupdate_value.to_string());
            }
        }
    }
    Ok(units)
}       

pub fn activate_quadlets(files: Vec<PathBuf>) -> Result<()> {
    let is_root = is_root();
    let target_dir = if cfg!(feature = "integration-tests") {
        PathBuf::from("/tmp/slater/containers/systemd")
    } else if is_root {
        PathBuf::from("/etc/containers/systemd")
    } else {
        let home = std::env::var("HOME").context("HOME environment variable not set")?;
        PathBuf::from(format!("{home}/.config/containers/systemd"))
    };

    let cwd = std::env::current_dir()?;

    let mut cmd = Command::new("/usr/lib/systemd/system-generators/podman-system-generator");
    cmd.arg("--dryrun");
    if !is_root {
        cmd.arg("--user");
    }
    cmd.env("QUADLET_UNIT_DIRS", &cwd);

    let output = cmd.output()?;
    if !output.status.success() {
        anyhow::bail!(
            "Validation command failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    println!("Generated systemd unit files (dry run):");
    println!("{}", String::from_utf8_lossy(&output.stdout));

    if cwd != target_dir
        && ask_confirm(
            &format!("Create symlinks in '{}'?", target_dir.display()),
            true,
        )? {
            std::fs::create_dir_all(&target_dir)?;

            for file_path in &files {
                let file_name = file_path.file_name()
                    .context("Failed to get filename from path")?;
                let src = cwd.join(file_name);
                let dst = target_dir.join(file_name);

                if dst.exists() {
                    if let Err(e) = std::fs::remove_file(&dst) {
                        error!("Failed to remove file {}: {}", dst.display(), e);
                        continue;
                    }
                }

                if let Err(e) = std::os::unix::fs::symlink(&src, &dst) {
                    error!("Failed to create symlink {} -> {}: {}", src.display(), dst.display(), e);
                    continue;
                }

                info!("Created symlink: {} -> {}", dst.display(), src.display());
            }
        }

    if ask_confirm("Reload systemd and restart the services?", true)? {
        systemctl_cmd(is_root).arg("daemon-reload").status()?;
        info!("systemctl-daemon reloaded!");

        for pod_path in files.iter().filter(|p| {
            p.extension().map(|ext| ext == "pod").unwrap_or(false)
        }) {
            let pod_name_stem = pod_path.file_stem()
                .and_then(|s| s.to_str())
                .context("Failed to get pod file stem")?;

            let pod_unit_name = format!("{pod_name_stem}-pod.service");

            systemctl_cmd(is_root)
                .arg("restart")
                .arg(&pod_unit_name)
                .status()?;
        }
    }

    Ok(())
}


#[cfg(test)]
mod tests {
    use crate::utils::enter_test_dir;

    use super::*;
    use std::{io::Write};

    fn setup_quadlets() -> IniFiles {
        let input = r#"
# bookstack-app.container
[Unit]
Requires=bookstack-db.service
After=bookstack-db.service

[Container]
Image=lscr.io/linuxserver/bookstack
Pod=bookstack.pod

[Service]
Restart=always

---

# bookstack-db.container
[Container]
Image=lscr.io/linuxserver/mariadb
Pod=bookstack.pod

[Service]
Restart=always

---

# bookstack.pod
[Pod]
PublishPort=127.0.0.1:11004:80
"#;
        parse_raw_quadlets(input.trim()).unwrap()
    }

    #[test]
    fn test_parse_raw_quadlets() {
        let result = setup_quadlets();

        let app_container = result.get("bookstack-app.container").unwrap();
        assert_eq!(
            app_container.get("Unit").unwrap().get("Requires"),
            Some(&"bookstack-db.service".to_string())
        );
        assert_eq!(
            app_container.get("Container").unwrap().get("Image"),
            Some(&"lscr.io/linuxserver/bookstack".to_string())
        );

        let db_container = result.get("bookstack-db.container").unwrap();
        assert_eq!(
            db_container.get("Container").unwrap().get("Image"),
            Some(&"lscr.io/linuxserver/mariadb".to_string())
        );

        let pod = result.get("bookstack.pod").unwrap();
        assert_eq!(
            pod.get("Pod").unwrap().get("PublishPort"),
            Some(&"127.0.0.1:11004:80".to_string())
        );
    }

    #[test]
    fn test_process_quadlets() {
        let quadlets = setup_quadlets();
        let dir = enter_test_dir();

        let env_path = std::env::current_dir().unwrap().join(".env");
        let mut env_file = std::fs::File::create(&env_path).unwrap();
        writeln!(env_file, "TEST_VAR=123").unwrap();

        let processed_quadlets = process_quadlets(quadlets, Some(&dir)).unwrap();
        for (name, i) in processed_quadlets.0 {
            insta::assert_snapshot!(
                format!("process_quadlets_{}", name),
                serde_ini::to_string(&i).unwrap()
            );
        }
    }

    #[test]
    fn test_parse_qualified_name() {
        let input = r#"[
        {
            "Ref": "docker.io/library/ubuntu:22.04@sha256:6f63292a7444f9346bf6ec6816dd93029dae021ee00cabb564c440417519680c"
        }
    ]"#;
        let expected = "docker.io/library/ubuntu:22.04";
        let result = parse_qualified_name(input.as_bytes()).unwrap();
        assert_eq!(result, expected);
    }
}
