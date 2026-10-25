"""Emit winget and scoop manifests for a packaged release.

Reads the package directory produced by audit/package_release.py (version, binary hash, the
windows-x64 zip and its SHA256SUMS entry) and writes the three winget files plus one scoop
manifest. Nothing is submitted anywhere: the files are the input for a winget-pkgs pull request
or a scoop bucket, which a maintainer opens by hand.
"""

import argparse
import hashlib
import io
import json
import re
from pathlib import Path

WINGET_PLACEHOLDER_DATE = "2026-10-26"


def sha256(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def read_version(package: Path) -> str:
    manifest = package / "windows-x64" / "manifest.json"
    return json.loads(manifest.read_text(encoding="utf8"))["version"]


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--package", required=True, help="release package directory")
    parser.add_argument("--id", default="Mothzip.Mothzip", help="winget package identifier")
    parser.add_argument("--publisher", default="Mothzip", help="winget publisher")
    parser.add_argument("--repo", default="ghdtjrtka/mothzip", help="GitHub owner/repo")
    parser.add_argument("--out", default="packaging/winget", help="output directory")
    arguments = parser.parse_args()

    package = Path(arguments.package).resolve()
    version = read_version(package)
    zip_path = package / f"mothzip-{version}-windows-x64.zip"
    if not zip_path.is_file():
        raise SystemExit(f"missing archive: {zip_path}")
    digest = sha256(zip_path)
    sums = (package / "SHA256SUMS.txt").read_text(encoding="utf8")
    if digest.upper() not in sums.upper():
        raise SystemExit("SHA256SUMS.txt does not list the archive digest")

    out = Path(arguments.out).resolve()
    out.mkdir(parents=True, exist_ok=True)
    package_id = arguments.id
    publisher = arguments.publisher
    url = f"https://github.com/{arguments.repo}/releases/download/v{version}/mothzip-{version}-windows-x64.zip"

    installer = f"""# winget create validate -- manifests; then open the pull request against microsoft/winget-pkgs
PackageIdentifier: {package_id}
PackageVersion: {version}
InstallerType: zip
NestedInstallerType: portable
NestedInstallerFiles:
- RelativeFilePath: mothzip.exe
  PortableCommandAlias: mothzip
Installers:
- Architecture: x64
  InstallerUrl: {url}
  InstallerSha256: {digest.upper()}
  Scope: user
ManifestType: installer
ManifestVersion: 1.6.0
"""
    (out / f"{package_id}.installer.yaml").write_text(installer, encoding="utf8")

    locale = f"""PackageIdentifier: {package_id}
PackageVersion: {version}
PackageLocale: en-US
Publisher: {publisher}
PublisherUrl: https://github.com/{arguments.repo}
PackageName: Mothzip
PackageUrl: https://github.com/{arguments.repo}
License: MIT
LicenseUrl: https://github.com/{arguments.repo}/blob/master/LICENSE
ShortDescription: Ad-free Windows archive CLI with JSON output for scripts and agents
Description: >-
  A single-executable Windows archive tool: ZIP (including AES), 7z, TAR and gzip/zstd/xz/lz4/brotli
  creation, RAR extraction, ZIP/7z editing, split volumes, SFX, reproducible archives and
  stdout/stdin streaming. Explorer integration adds a three-item context menu and a double-click
  browser; the command line reports JSON, uses exit codes 0/1/2 and never prompts.
Tags:
- archiver
- cli
- zip
- 7zip
- tar
- zstd
ManifestType: defaultLocale
ManifestVersion: 1.6.0
"""
    (out / f"{package_id}.locale.en-US.yaml").write_text(locale, encoding="utf8")

    version_manifest = f"""PackageIdentifier: {package_id}
PackageVersion: {version}
DefaultLocale: en-US
ManifestType: version
ManifestVersion: 1.6.0
"""
    (out / f"{package_id}.yaml").write_text(version_manifest, encoding="utf8")

    scoop = {
        "version": version,
        "description": "Ad-free Windows archive CLI with JSON output for scripts and agents",
        "homepage": f"https://github.com/{arguments.repo}",
        "license": "MIT",
        "architecture": {
            "64bit": {
                "url": f"https://github.com/{arguments.repo}/releases/download/v{version}/mothzip-{version}-windows-x64.zip",
                "hash": digest,
            }
        },
        "bin": "mothzip.exe",
        "checkver": {"github": f"https://github.com/{arguments.repo}"},
        "autoupdate": {
            "architecture": {
                "64bit": {
                    "url": f"https://github.com/{arguments.repo}/releases/download/v$version/mothzip-$version-windows-x64.zip"
                }
            }
        },
    }
    scoop_out = out.parent / "scoop"
    scoop_out.mkdir(parents=True, exist_ok=True)
    (scoop_out / "mothzip.json").write_text(json.dumps(scoop, indent=2) + "\n", encoding="utf8")

    print(
        json.dumps(
            {
                "version": version,
                "installer_sha256": digest,
                "winget_files": [
                    f"{package_id}.yaml",
                    f"{package_id}.installer.yaml",
                    f"{package_id}.locale.en-US.yaml",
                ],
                "scoop_manifest": str(scoop_out / "mothzip.json"),
            },
            indent=2,
        )
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
