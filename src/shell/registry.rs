use std::env;
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use super::error::ShellError;
use super::system::{get_reg_exe, run_powershell_script};

pub use crate::core::format::SUPPORTED_EXTENSIONS;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegValue {
    Sz(String),
    Dword(u32),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegKeySpec {
    pub key: String,
    pub values: Vec<(String, RegValue)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistryProfile {
    pub keys: Vec<RegKeySpec>,
    pub shared_values: Vec<(String, String, RegValue)>,
    pub is_managed: bool,
    pub exe_path: PathBuf,
}

/// Formats offered by the Explorer "압축하기" submenu: verb id, menu label, archive suffix.
/// Single-stream formats accept exactly one selected file; the CLI refuses anything else with
/// the reason, so the menu itself stays static.
const PACK_FORMATS: &[(&str, &str, &str)] = &[
    ("Zip", "ZIP (.zip)", "zip"),
    ("SevenZ", "7Z (.7z)", "7z"),
    ("Tar", "TAR (.tar)", "tar"),
    ("TarGz", "TAR.GZ (.tar.gz)", "tar.gz"),
    ("TarZst", "TAR.ZST (.tar.zst)", "tar.zst"),
    ("TarXz", "TAR.XZ (.tar.xz)", "tar.xz"),
    ("TarLz4", "TAR.LZ4 (.tar.lz4)", "tar.lz4"),
    ("TarBr", "TAR.BR (.tar.br)", "tar.br"),
    ("Gz", "GZIP (.gz)", "gz"),
    ("Zst", "Zstandard (.zst)", "zst"),
    ("Xz", "XZ (.xz)", "xz"),
    ("Lz4", "LZ4 (.lz4)", "lz4"),
    ("Br", "Brotli (.br)", "br"),
];

/// Verb keys that 1.0.0 wrote and 1.1.x replaces with the single "알아서 풀기" entry. They are
/// deleted on install and on uninstall, so an upgraded machine keeps only the current menu.
pub fn legacy_verb_keys(extensions: &[&str]) -> Vec<String> {
    let mut keys = Vec::new();
    for ext in extensions {
        for prefix in [
            format!("HKEY_CURRENT_USER\\Software\\Classes\\{ext}"),
            format!("HKEY_CURRENT_USER\\Software\\Classes\\SystemFileAssociations\\{ext}"),
        ] {
            for verb in ["MothZip.UnpackHere", "MothZip.UnpackFolder"] {
                keys.push(format!("{prefix}\\shell\\{verb}"));
            }
        }
    }
    keys
}

/// Delete the verb keys of earlier releases. Only MothZip-owned verb subtrees are removed.
pub fn remove_legacy_verbs(extensions: &[&str]) -> Result<Vec<String>, ShellError> {
    let keys = legacy_verb_keys(extensions);
    let mut content = String::from("Windows Registry Editor Version 5.00\r\n\r\n");
    for key in &keys {
        content.push_str(&format!("[-{key}]\r\n"));
    }
    let temp_reg = unique_temp_path("mothzip_legacy_", "reg");
    write_reg_file(&temp_reg, &content)?;
    let result = import_reg_file(&temp_reg);
    let _ = fs::remove_file(&temp_reg);
    result?;
    Ok(keys)
}

/// Escape a string value for serialization inside a .reg file:
/// backslashes and double quotes are the two escape characters of the format.
fn escape_reg_data(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Escape a value-name for serialization inside a .reg file.
fn escape_reg_name(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

impl RegistryProfile {
    /// Build the single unified declarative registry profile for MothZip.
    /// Values are stored RAW (unescaped); escaping happens only in .reg serialization.
    pub fn build(exe_path: &Path, is_managed: bool, extensions: &[&str]) -> Self {
        let exe_str = exe_path.to_string_lossy().to_string();
        let exe_dir = exe_path
            .parent()
            .unwrap_or(exe_path)
            .to_string_lossy()
            .to_string();

        let version = env!("CARGO_PKG_VERSION");
        let (uninstall_cmd, quiet_uninstall_cmd) = if is_managed {
            (
                format!("\"{exe_str}\" uninstall"),
                format!("\"{exe_str}\" uninstall --quiet"),
            )
        } else {
            (
                format!("\"{exe_str}\" unregister"),
                format!("\"{exe_str}\" unregister"),
            )
        };

        let applies_to = extensions
            .iter()
            .map(|ext| format!("System.FileExtension:<>\"{ext}\""))
            .collect::<Vec<_>>()
            .join(" AND ");

        let icon = format!("\"{exe_str}\",0");
        let unpack_command = format!("\"{exe_str}\" smart-unpack \"%1\" --open");
        // Double-click opens the browsing window; extraction stays on the explicit verb.
        let browse_command = format!("\"{exe_str}\" open \"%1\"");
        let pack_auto = |suffix: &str| format!("\"{exe_str}\" pack-auto --format {suffix} \"%1\"");

        let mut keys = Vec::new();

        // 1. Extraction verb on archive extensions: exactly one "알아서 풀기" entry. Both the
        //    direct Classes\{ext} path and SystemFileAssociations are written for shell coverage.
        for ext in extensions {
            for prefix in &[
                format!("HKEY_CURRENT_USER\\Software\\Classes\\{ext}"),
                format!("HKEY_CURRENT_USER\\Software\\Classes\\SystemFileAssociations\\{ext}"),
            ] {
                keys.push(RegKeySpec {
                    key: format!("{prefix}\\shell\\MothZip.SmartUnpack"),
                    values: vec![
                        ("".into(), RegValue::Sz("알아서 풀기 (&MothZip)".into())),
                        ("Icon".into(), RegValue::Sz(icon.clone())),
                        ("MultiSelectModel".into(), RegValue::Sz("Player".into())),
                    ],
                });
                keys.push(RegKeySpec {
                    key: format!("{prefix}\\shell\\MothZip.SmartUnpack\\command"),
                    values: vec![("".into(), RegValue::Sz(unpack_command.clone()))],
                });
            }
        }

        // 2. Creation verbs: a one-click ZIP entry plus a cascading "압축하기" submenu that lists
        //    every writable format. Registered for files and for directories.
        for root in [
            "HKEY_CURRENT_USER\\Software\\Classes\\*",
            "HKEY_CURRENT_USER\\Software\\Classes\\Directory",
        ] {
            keys.push(RegKeySpec {
                key: format!("{root}\\shell\\MothZip.PackZip"),
                values: vec![
                    ("".into(), RegValue::Sz("ZIP으로 압축 (&MothZip)".into())),
                    ("Icon".into(), RegValue::Sz(icon.clone())),
                    ("MultiSelectModel".into(), RegValue::Sz("Player".into())),
                    ("AppliesTo".into(), RegValue::Sz(applies_to.clone())),
                ],
            });
            keys.push(RegKeySpec {
                key: format!("{root}\\shell\\MothZip.PackZip\\command"),
                values: vec![("".into(), RegValue::Sz(pack_auto("zip")))],
            });

            keys.push(RegKeySpec {
                key: format!("{root}\\shell\\MothZip.Pack"),
                values: vec![
                    ("".into(), RegValue::Sz("압축하기 (&MothZip)".into())),
                    ("Icon".into(), RegValue::Sz(icon.clone())),
                    ("MultiSelectModel".into(), RegValue::Sz("Player".into())),
                    ("AppliesTo".into(), RegValue::Sz(applies_to.clone())),
                    // An empty SubCommands value plus child verbs under shell\ makes Explorer
                    // render the formats as a cascading submenu instead of running a command.
                    ("SubCommands".into(), RegValue::Sz(String::new())),
                ],
            });
            for (verb, label, suffix) in PACK_FORMATS {
                keys.push(RegKeySpec {
                    key: format!("{root}\\shell\\MothZip.Pack\\shell\\MothZip.Pack.{verb}"),
                    values: vec![
                        ("".into(), RegValue::Sz((*label).into())),
                        ("Icon".into(), RegValue::Sz(icon.clone())),
                    ],
                });
                keys.push(RegKeySpec {
                    key: format!(
                        "{root}\\shell\\MothZip.Pack\\shell\\MothZip.Pack.{verb}\\command"
                    ),
                    values: vec![("".into(), RegValue::Sz(pack_auto(suffix)))],
                });
            }
        }

        // 4. Installed Application (Settings / Control Panel)
        keys.push(RegKeySpec {
            key: "HKEY_CURRENT_USER\\Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\MothZip".into(),
            values: vec![
                ("DisplayName".into(), RegValue::Sz("MothZip (나방집)".into())),
                ("DisplayIcon".into(), RegValue::Sz(icon.clone())),
                ("DisplayVersion".into(), RegValue::Sz(version.into())),
                ("Publisher".into(), RegValue::Sz("mothzip contributors".into())),
                ("InstallLocation".into(), RegValue::Sz(exe_dir.clone())),
                ("UninstallString".into(), RegValue::Sz(uninstall_cmd)),
                ("QuietUninstallString".into(), RegValue::Sz(quiet_uninstall_cmd)),
                ("Comments".into(), RegValue::Sz("초경량 무광고 에이전틱 압축 프로그램 (ZIP, TAR, GZ, 7Z, RAR 지원)".into())),
                ("NoModify".into(), RegValue::Dword(1)),
                ("NoRepair".into(), RegValue::Dword(1)),
            ],
        });

        // 5. App Paths (Run dialog Win+R -> 'mothzip')
        keys.push(RegKeySpec {
            key: "HKEY_CURRENT_USER\\Software\\Microsoft\\Windows\\CurrentVersion\\App Paths\\mothzip.exe".into(),
            values: vec![
                ("".into(), RegValue::Sz(exe_str.clone())),
                ("Path".into(), RegValue::Sz(exe_dir.clone())),
            ],
        });

        // 6. Applications (Open With)
        let mut app_types = Vec::new();
        for ext in extensions {
            app_types.push((ext.to_string(), RegValue::Sz("".into())));
        }
        keys.push(RegKeySpec {
            key: "HKEY_CURRENT_USER\\Software\\Classes\\Applications\\mothzip.exe".into(),
            values: vec![(
                "FriendlyAppName".into(),
                RegValue::Sz("MothZip (나방집)".into()),
            )],
        });
        keys.push(RegKeySpec {
            key: "HKEY_CURRENT_USER\\Software\\Classes\\Applications\\mothzip.exe\\DefaultIcon"
                .into(),
            values: vec![("".into(), RegValue::Sz(icon.clone()))],
        });
        keys.push(RegKeySpec {
            key: "HKEY_CURRENT_USER\\Software\\Classes\\Applications\\mothzip.exe\\shell\\open\\command".into(),
            values: vec![("".into(), RegValue::Sz(unpack_command.clone()))],
        });
        keys.push(RegKeySpec {
            key: "HKEY_CURRENT_USER\\Software\\Classes\\Applications\\mothzip.exe\\SupportedTypes"
                .into(),
            values: app_types,
        });

        // 7. Capabilities & FileAssociations
        let mut file_assocs = Vec::new();
        for ext in extensions {
            file_assocs.push((ext.to_string(), RegValue::Sz("MothZip.Archive".into())));
        }
        keys.push(RegKeySpec {
            key: "HKEY_CURRENT_USER\\Software\\MothZip\\Capabilities".into(),
            values: vec![
                ("ApplicationName".into(), RegValue::Sz("MothZip".into())),
                (
                    "ApplicationDescription".into(),
                    RegValue::Sz("MothZip - 초경량 무광고 에이전틱 압축 프로그램".into()),
                ),
            ],
        });
        keys.push(RegKeySpec {
            key: "HKEY_CURRENT_USER\\Software\\MothZip\\Capabilities\\FileAssociations".into(),
            values: file_assocs,
        });

        // 8. ProgId
        keys.push(RegKeySpec {
            key: "HKEY_CURRENT_USER\\Software\\Classes\\MothZip.Archive".into(),
            values: vec![("".into(), RegValue::Sz("MothZip 압축 파일".into()))],
        });
        keys.push(RegKeySpec {
            key: "HKEY_CURRENT_USER\\Software\\Classes\\MothZip.Archive\\DefaultIcon".into(),
            values: vec![("".into(), RegValue::Sz(icon.clone()))],
        });
        keys.push(RegKeySpec {
            key: "HKEY_CURRENT_USER\\Software\\Classes\\MothZip.Archive\\shell\\open\\command"
                .into(),
            values: vec![("".into(), RegValue::Sz(browse_command.clone()))],
        });

        let shared_values = vec![(
            "HKEY_CURRENT_USER\\Software\\RegisteredApplications".into(),
            "MothZip".into(),
            RegValue::Sz("Software\\MothZip\\Capabilities".into()),
        )];

        Self {
            keys,
            shared_values,
            is_managed,
            exe_path: exe_path.to_path_buf(),
        }
    }

    /// Generate standard Windows .reg file content from declarative specification.
    /// This is the ONLY place that escapes raw stored values.
    pub fn generate_reg_content(&self) -> String {
        let mut reg = String::from("Windows Registry Editor Version 5.00\r\n\r\n");

        for spec in &self.keys {
            reg.push_str(&format!("[{}]\r\n", spec.key));
            for (name, val) in &spec.values {
                let name_part = if name.is_empty() {
                    "@".into()
                } else {
                    format!("\"{}\"", escape_reg_name(name))
                };
                match val {
                    RegValue::Sz(s) => {
                        reg.push_str(&format!("{name_part}=\"{}\"\r\n", escape_reg_data(s)))
                    }
                    RegValue::Dword(d) => reg.push_str(&format!("{name_part}=dword:{d:08x}\r\n")),
                }
            }
            reg.push_str("\r\n");
        }

        for (key, val_name, val_data) in &self.shared_values {
            reg.push_str(&format!("[{key}]\r\n"));
            match val_data {
                RegValue::Sz(s) => reg.push_str(&format!(
                    "\"{}\"=\"{}\"\r\n\r\n",
                    escape_reg_name(val_name),
                    escape_reg_data(s)
                )),
                RegValue::Dword(d) => reg.push_str(&format!(
                    "\"{}\"=dword:{d:08x}\r\n\r\n",
                    escape_reg_name(val_name)
                )),
            }
        }

        reg
    }

    /// Derive unregistration script: deletes exclusively owned keys, and only values for shared keys
    pub fn generate_unreg_content(&self) -> String {
        let mut reg = String::from("Windows Registry Editor Version 5.00\r\n\r\n");

        for spec in &self.keys {
            reg.push_str(&format!("[-{}]\r\n", spec.key));
        }

        for (key, val_name, _) in &self.shared_values {
            reg.push_str(&format!(
                "[{key}]\r\n\"{}\"=-\r\n\r\n",
                escape_reg_name(val_name)
            ));
        }

        reg
    }

    /// A JSON list of every declared key/value and shared value that must be present.
    fn build_verify_plan(&self) -> serde_json::Value {
        let keys: Vec<serde_json::Value> = self
            .keys
            .iter()
            .map(|spec| {
                serde_json::json!({
                    "key": spec.key,
                    "values": spec.values.iter().map(|(name, val)| match val {
                        RegValue::Sz(s) => serde_json::json!({
                            "name": name, "kind": "SZ", "value": s
                        }),
                        RegValue::Dword(d) => serde_json::json!({
                            "name": name, "kind": "DWORD", "value": d
                        }),
                    }).collect::<Vec<_>>(),
                })
            })
            .collect();

        let shared_values: Vec<serde_json::Value> = self
            .shared_values
            .iter()
            .map(|(key, name, val)| match val {
                RegValue::Sz(s) => serde_json::json!({
                    "key": key, "name": name, "kind": "SZ", "value": s
                }),
                RegValue::Dword(d) => serde_json::json!({
                    "key": key, "name": name, "kind": "DWORD", "value": d
                }),
            })
            .collect();

        serde_json::json!({ "keys": keys, "shared_values": shared_values })
    }

    /// Real verification of registered keys and exact data/kind against host registry.
    /// A single PowerShell process consumes a JSON plan file (never a giant command line),
    /// preserving Unicode and avoiding Windows command-line length limits.
    pub fn verify_registered(&self) -> Result<bool, ShellError> {
        let plan = self.build_verify_plan();
        let plan_json = serde_json::to_string(&plan)?;
        let plan_path = unique_temp_path("mothzip_verify_", "json");
        write_temp_json(&plan_path, &plan_json)?;

        let script = r#"
param([string]$PlanPath)
$ErrorActionPreference = 'Stop'
[Console]::OutputEncoding = [System.Text.Encoding]::UTF8

$plan = Get-Content -LiteralPath $PlanPath -Raw -Encoding UTF8 | ConvertFrom-Json

function Test-RegValue($key, $name, $kind, $expected) {
    if ($null -eq $key) { return "MISSING_KEY" }
    if (-not (@($key.GetValueNames()) -contains [string]$name)) { return "MISSING_VALUE" }
    $actualKind = $key.GetValueKind([string]$name).ToString()
    $wantedKind = if ($kind -eq 'DWORD') { 'DWord' } else { 'String' }
    if ($actualKind -ne $wantedKind) { return "KIND_MISMATCH" }
    $got = $key.GetValue([string]$name, $null, [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames)
    if ($kind -eq 'DWORD') {
        if ([int64]$got -ne [int64]$expected) { return "VALUE_MISMATCH" }
    } elseif (-not [string]::Equals([string]$got,[string]$expected,[StringComparison]::Ordinal)) { return "VALUE_MISMATCH" }
    return $null
}

foreach ($spec in $plan.keys) {
    $prop = Get-Item -LiteralPath ('Registry::' + $spec.key) -ErrorAction SilentlyContinue
    foreach ($v in $spec.values) {
        $r = Test-RegValue $prop $v.name $v.kind $v.value
        if ($null -ne $r) {
            Write-Output ("FAIL:" + $r + ":" + $spec.key)
            exit 0
        }
    }
}

foreach ($sv in $plan.shared_values) {
    $prop = Get-Item -LiteralPath ('Registry::' + $sv.key) -ErrorAction SilentlyContinue
    $r = Test-RegValue $prop $sv.name $sv.kind $sv.value
    if ($null -ne $r) {
        Write-Output ("FAIL:" + $r + ":" + $sv.key)
        exit 0
    }
}

Write-Output "OK"
"#;

        let out = run_powershell_script(script, &[&plan_path.to_string_lossy()]);
        let _ = fs::remove_file(&plan_path);
        let out = out?;
        Ok(out.trim() == "OK")
    }

    /// Ownership of the existing registration: exact canonical exe match, never a substring match.
    pub fn verify_ownership(&self, expected_exe: &Path) -> Result<Ownership, ShellError> {
        let ownership_script = r#"
param([string]$ExpectedExe)
$ErrorActionPreference = 'Stop'
[Console]::OutputEncoding = [System.Text.Encoding]::UTF8

function Get-CanonPath($p) {
    if ([string]::IsNullOrWhiteSpace($p)) { return $null }
    try {
        $full = [IO.Path]::GetFullPath($p.Trim().Trim('"'))
        return $full.Replace('/','\').ToLowerInvariant()
    } catch { return $null }
}

$appPathKey = "HKCU:\Software\Microsoft\Windows\CurrentVersion\App Paths\mothzip.exe"
$uninstKey = "HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\MothZip"

$ap = Get-ItemProperty -LiteralPath $appPathKey -ErrorAction SilentlyContinue
$un = Get-ItemProperty -LiteralPath $uninstKey -ErrorAction SilentlyContinue

if ($null -eq $ap -and $null -eq $un) {
    Write-Output "NONE"
    exit 0
}

$expectedNorm = Get-CanonPath $ExpectedExe
if ($null -eq $expectedNorm) {
    Write-Output "FOREIGN"
    exit 0
}

if ($null -ne $ap) {
    $found = Get-CanonPath ($ap.PSObject.Properties['(default)'].Value)
    if ($found -eq $expectedNorm) {
        Write-Output "OWNED"
        exit 0
    }
}

if ($null -ne $un) {
    $str = [string]$un.UninstallString
    $m = [regex]::Match($str, '^"(.*?)"')
    if ($m.Success) {
        $found2 = Get-CanonPath $m.Groups[1].Value
        if ($found2 -eq $expectedNorm) {
            Write-Output "OWNED"
            exit 0
        }
    }
}

Write-Output "FOREIGN"
"#;

        let exe_str = expected_exe.to_string_lossy();
        let res = run_powershell_script(ownership_script, &[&exe_str])?;

        match res.trim() {
            "OWNED" => Ok(Ownership::Owned),
            "NONE" => Ok(Ownership::None),
            _ => Ok(Ownership::Foreign),
        }
    }

    /// Capture the current state of only this profile's declared keys/values so an
    /// install transaction can roll them back. Does not touch unrelated user settings.
    /// Consumed by the install transaction in installer.rs (root).
    pub fn snapshot(&self) -> Result<RegistrySnapshot, ShellError> {
        let mut specs: Vec<serde_json::Value> = self.keys.iter().map(|spec| serde_json::json!({
            "key":spec.key,"shared":false,"names":spec.values.iter().map(|(name,_)|name).collect::<Vec<_>>()
        })).collect();
        specs.extend(
            self.shared_values
                .iter()
                .map(|(key, name, _)| serde_json::json!({"key":key,"shared":true,"names":[name]})),
        );
        let plan_path = unique_temp_path("mothzip_snapshot_", "json");
        write_temp_json(&plan_path, &serde_json::to_string(&specs)?)?;
        let script = r#"
param([string]$PlanPath)
$plan = @(Get-Content -LiteralPath $PlanPath -Raw -Encoding UTF8 | ConvertFrom-Json)
$result = @()
foreach ($spec in $plan) {
    $path = 'Registry::' + $spec.key
    $key = Get-Item -LiteralPath $path -ErrorAction SilentlyContinue
    foreach ($name in $spec.names) {
        $exists = $false; $kind = $null; $value = $null
        if ($null -ne $key) {
            $exists = @($key.GetValueNames()) -contains [string]$name
            if ($exists) {
                $kind = $key.GetValueKind([string]$name).ToString()
                if ($kind -notin @('String','DWord')) { throw "Unsupported prior registry kind: $kind at $path" }
                $value = $key.GetValue([string]$name, $null, [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames)
            }
        }
        $result += [PSCustomObject]@{ key=$spec.key; name=[string]$name; key_existed=($null -ne $key); shared=[bool]$spec.shared; kind=$kind; value=$value }
    }
}
ConvertTo-Json -InputObject @($result) -Depth 5 -Compress
"#;
        let output = run_powershell_script(script, &[&plan_path.to_string_lossy()]);
        let _ = fs::remove_file(&plan_path);
        let value: serde_json::Value = serde_json::from_str(&output?)?;
        let mut entries = Vec::new();
        for item in value.as_array().ok_or_else(|| ShellError::Validation {
            field: "registry snapshot".into(),
            message: "Expected array".into(),
        })? {
            let entry_value = match item["kind"].as_str() {
                Some("DWord") => Some(RegValue::Dword(item["value"].as_i64().ok_or_else(|| {
                    ShellError::Validation {
                        field: "registry snapshot".into(),
                        message: "Invalid DWORD".into(),
                    }
                })? as u32)),
                Some("String") => Some(RegValue::Sz(
                    item["value"]
                        .as_str()
                        .ok_or_else(|| ShellError::Validation {
                            field: "registry snapshot".into(),
                            message: "Invalid string".into(),
                        })?
                        .into(),
                )),
                _ => None,
            };
            entries.push(SnapshotEntry {
                key: item["key"].as_str().unwrap_or_default().into(),
                name: item["name"].as_str().unwrap_or_default().into(),
                value: entry_value,
                key_existed: item["key_existed"].as_bool() == Some(true),
                shared: item["shared"].as_bool() == Some(true),
            });
        }
        Ok(RegistrySnapshot { entries })
    }

    /// Restore a previously captured snapshot using a narrow .reg write/delete.
    /// Consumed by the install transaction in installer.rs (root).
    pub fn restore(&self, snapshot: &RegistrySnapshot) -> Result<(), ShellError> {
        let mut reg = String::from("Windows Registry Editor Version 5.00\r\n\r\n");
        for entry in &snapshot.entries {
            if !entry.key_existed && !entry.shared {
                reg.push_str(&format!("[-{}]\r\n", entry.key));
                continue;
            }
            reg.push_str(&format!("[{}]\r\n", entry.key));
            let name_part = if entry.name.is_empty() {
                "@".into()
            } else {
                format!("\"{}\"", escape_reg_name(&entry.name))
            };
            match &entry.value {
                Some(RegValue::Sz(s)) => {
                    reg.push_str(&format!("{name_part}=\"{}\"\r\n\r\n", escape_reg_data(s)))
                }
                Some(RegValue::Dword(d)) => {
                    reg.push_str(&format!("{name_part}=dword:{d:08x}\r\n\r\n"))
                }
                None => {
                    reg.push_str(&format!("{name_part}=-\r\n\r\n"));
                }
            }
        }

        let temp_reg = unique_temp_path("mothzip_restore_", "reg");
        write_reg_file(&temp_reg, &reg)?;
        let result = import_reg_file(&temp_reg);
        let _ = fs::remove_file(&temp_reg);
        result
    }
}

/// Ownership classification of the current registry registration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ownership {
    Owned,
    Foreign,
    None,
}

/// A single captured (key, value-name) registry state.
#[derive(Debug, Clone)]
pub struct SnapshotEntry {
    pub key: String,
    pub name: String,
    pub value: Option<RegValue>,
    pub key_existed: bool,
    pub shared: bool,
}

/// Narrow rollback snapshot of this profile's declared app values.
#[derive(Debug, Clone, Default)]
pub struct RegistrySnapshot {
    pub entries: Vec<SnapshotEntry>,
}

/// Generate a unique temp-file path in the system temp directory.
fn unique_temp_path(prefix: &str, ext: &str) -> PathBuf {
    let id = format!(
        "{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    );
    env::temp_dir().join(format!("{prefix}{id}.{ext}"))
}

/// Atomically write a UTF-8 JSON temp file, failing if the path already exists.
fn write_temp_json(path: &Path, content: &str) -> Result<(), ShellError> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    let mut f = options.open(path)?;
    f.write_all(content.as_bytes())?;
    f.sync_all()?;
    Ok(())
}

/// Write standard Windows Unicode (UTF-16LE with BOM) .reg file
pub fn write_reg_file(path: &Path, content: &str) -> io::Result<()> {
    let mut file = File::create(path)?;
    file.write_all(&[0xFF, 0xFE])?;
    for u in content.encode_utf16() {
        file.write_all(&u.to_le_bytes())?;
    }
    file.sync_all()?;
    Ok(())
}

/// Import a .reg file using Windows system reg.exe
pub fn import_reg_file(path: &Path) -> Result<(), ShellError> {
    let reg_exe = get_reg_exe()?;
    let output = Command::new(&reg_exe)
        .args(["import", &path.to_string_lossy()])
        .output()?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(ShellError::Registry {
            command: format!("{} import \"{}\"", reg_exe.display(), path.display()),
            exit_code: output.status.code(),
            stderr,
        });
    }

    Ok(())
}

/// Unregister all MothZip keys, strictly validating ownership before deletion.
pub fn unregister_all_keys(
    expected_exe: &Path,
    extensions: &[&str],
) -> Result<Vec<String>, ShellError> {
    let profile = RegistryProfile::build(expected_exe, false, extensions);

    match profile.verify_ownership(expected_exe)? {
        Ownership::None => Ok(Vec::new()),
        Ownership::Foreign => Err(ShellError::SecurityViolation {
            path: expected_exe.to_path_buf(),
            reason: "Registry entries exist but do not belong to the target MothZip executable"
                .into(),
        }),
        Ownership::Owned => {
            let unreg_content = profile.generate_unreg_content();
            let temp_reg = unique_temp_path("mothzip_unreg_", "reg");
            write_reg_file(&temp_reg, &unreg_content)?;

            let result = import_reg_file(&temp_reg);
            let _ = fs::remove_file(&temp_reg);
            result?;

            let removed = profile.keys.into_iter().map(|k| k.key).collect();
            Ok(removed)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_registry_profile_generation() {
        let dummy = Path::new("C:\\Program Files\\MothZip\\mothzip.exe");
        let profile = RegistryProfile::build(dummy, true, SUPPORTED_EXTENSIONS);
        let content = profile.generate_reg_content();

        assert!(content.contains("알아서 풀기 (&MothZip)"));
        assert!(content.contains("smart-unpack \\\"%1\\\" --open"));
        assert!(content.contains("uninstall --quiet"));

        for ext in SUPPORTED_EXTENSIONS {
            assert!(content.contains(&format!("Classes\\{ext}\\shell")));
            assert!(content.contains(&format!("SystemFileAssociations\\{ext}\\shell")));
        }
    }

    /// 1.2.0 menu contract: one extraction verb and no retired verbs; the creation side is a
    /// one-click ZIP verb plus a cascading submenu that names every writable format.
    #[test]
    fn test_registry_profile_pack_menu() {
        let dummy = Path::new("C:\\Program Files\\MothZip\\mothzip.exe");
        let profile = RegistryProfile::build(dummy, true, SUPPORTED_EXTENSIONS);
        let content = profile.generate_reg_content();

        assert!(!content.contains("MothZip.UnpackHere"));
        assert!(!content.contains("MothZip.UnpackFolder"));
        assert!(content.contains("ZIP으로 압축 (&MothZip)"));
        assert!(content.contains("압축하기 (&MothZip)"));

        for root in ["*", "Directory"] {
            let pack_key =
                format!("HKEY_CURRENT_USER\\Software\\Classes\\{root}\\shell\\MothZip.Pack");
            let spec = profile
                .keys
                .iter()
                .find(|k| k.key == pack_key)
                .unwrap_or_else(|| panic!("{pack_key} missing"));
            assert!(
                spec.values
                    .iter()
                    .any(|(name, value)| name == "SubCommands"
                        && *value == RegValue::Sz(String::new())),
                "cascading submenu marker missing on {root}"
            );
            for (verb, _, suffix) in PACK_FORMATS {
                let child = format!("{pack_key}\\shell\\MothZip.Pack.{verb}\\command");
                let command = profile
                    .keys
                    .iter()
                    .find(|k| k.key == child)
                    .unwrap_or_else(|| panic!("{child} missing"));
                assert!(
                    command
                        .values
                        .iter()
                        .any(|(_, value)| matches!(value, RegValue::Sz(text) if text.contains(&format!("pack-auto --format {suffix}")))),
                    "command for {verb} does not pass --format {suffix}"
                );
            }
        }
    }

    /// The retired verbs must be deleted on install, not only left out of the new profile.
    #[test]
    fn test_legacy_verbs_are_deleted() {
        let keys = legacy_verb_keys(SUPPORTED_EXTENSIONS);
        assert_eq!(keys.len(), SUPPORTED_EXTENSIONS.len() * 4);
        assert!(keys
            .iter()
            .any(|key| key.ends_with("Classes\\.zip\\shell\\MothZip.UnpackHere")));
        assert!(keys
            .iter()
            .any(|key| key.ends_with("SystemFileAssociations\\.7z\\shell\\MothZip.UnpackFolder")));
    }

    #[test]
    fn test_build_stores_raw_unescaped_values() {
        let dummy = Path::new("C:\\Program Files\\MothZip (나방집)\\mothzip.exe");
        let profile = RegistryProfile::build(dummy, true, SUPPORTED_EXTENSIONS);

        // UninstallString must be stored raw (single backslashes, real quotes).
        let uninstall = profile
            .keys
            .iter()
            .find(|k| k.key.ends_with("Uninstall\\MothZip"))
            .expect("uninstall key present");
        let cmd = uninstall
            .values
            .iter()
            .find(|(n, _)| n == "UninstallString")
            .expect("UninstallString value");
        assert!(matches!(
            cmd.1,
            RegValue::Sz(ref s) if s == "\"C:\\Program Files\\MothZip (나방집)\\mothzip.exe\" uninstall"
        ));

        // InstallLocation must be a bare path (no surrounding quotes).
        let loc = uninstall
            .values
            .iter()
            .find(|(n, _)| n == "InstallLocation")
            .expect("InstallLocation value");
        assert!(matches!(
            loc.1,
            RegValue::Sz(ref s) if s == "C:\\Program Files\\MothZip (나방집)"
        ));

        // App Paths default and Path must be bare paths.
        let app_paths = profile
            .keys
            .iter()
            .find(|k| k.key.ends_with("App Paths\\mothzip.exe"))
            .expect("app paths key present");
        let default_val = app_paths.values[0].clone();
        let path_val = app_paths.values[1].clone();
        assert_eq!(
            default_val.1,
            RegValue::Sz(dummy.to_string_lossy().to_string())
        );
        assert_eq!(
            path_val.1,
            RegValue::Sz("C:\\Program Files\\MothZip (나방집)".to_string())
        );
    }

    #[test]
    fn test_unverified_helplink_removed() {
        let dummy = Path::new("C:\\Program Files\\MothZip\\mothzip.exe");
        let profile = RegistryProfile::build(dummy, true, SUPPORTED_EXTENSIONS);
        let content = profile.generate_reg_content();
        assert!(!content.contains("HelpLink"));
        assert!(!content.contains("github.com"));
    }

    #[test]
    fn test_verify_plan_shape() {
        let dummy = Path::new("C:\\Program Files\\MothZip\\mothzip.exe");
        let profile = RegistryProfile::build(dummy, true, SUPPORTED_EXTENSIONS);
        let plan = profile.build_verify_plan();
        let keys = plan["keys"].as_array().expect("keys array");
        assert!(!keys.is_empty());
        // shared value present
        assert_eq!(plan["shared_values"].as_array().map(|a| a.len()), Some(1));
        // every declared value serialized with kind
        for spec in keys {
            for v in spec["values"].as_array().expect("values array") {
                let kind = v["kind"].as_str().expect("kind");
                assert!(kind == "SZ" || kind == "DWORD");
            }
        }
    }

    #[test]
    fn test_unreg_shared_key_preservation() {
        let dummy = Path::new("C:\\Program Files\\MothZip\\mothzip.exe");
        let profile = RegistryProfile::build(dummy, true, SUPPORTED_EXTENSIONS);
        let unreg = profile.generate_unreg_content();

        assert!(
            unreg.contains("[-HKEY_CURRENT_USER\\Software\\Classes\\Applications\\mothzip.exe]")
        );
        assert!(unreg.contains("[-HKEY_CURRENT_USER\\Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\MothZip]"));

        assert!(!unreg.contains("[-HKEY_CURRENT_USER\\Software\\RegisteredApplications]"));
        assert!(unreg
            .contains("[HKEY_CURRENT_USER\\Software\\RegisteredApplications]\r\n\"MothZip\"=-"));
    }
}
