# Mothzip file icons

The existing Mothzip mark is reused with a format label below it. The app's original icon remains unchanged. Compound suffixes use two label lines at larger sizes; small icons show the final codec label with a pixel alphabet for legibility.

Regenerate from the repository root on Windows:

```powershell
.\tools\generate-file-icons.ps1
```

`src/core/file_types.rs` is the shared suffix and resource-ID catalog for generation, the resource compiler, and file association registration. Every icon contains 16/20/24/32/40/48/64/96/128/256 pixel PNG frames. SVG layout files reference the unchanged `../moth_icon.png`; the PowerShell generator is the canonical renderer for pixel-size adjustments.

The generic archive icon uses resource 100; format icons use the catalog's `file_icon_resource_id`. `build.rs` embeds all groups in mothzip.exe. Windows registration uses negative resource IDs, not icon order, and a distinct Mothzip ProgID for each suffix. The old shared ProgID remains as a generic archive handler for compatibility.

Explorer uses the effective Windows file association. Select Mothzip in Windows Default apps to use these file icons. Installing icon resources does not rewrite protected UserChoice settings. Compound filenames can use their final extension's association; distinguishing them dynamically would require a separate shell icon handler.
