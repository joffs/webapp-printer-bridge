# WebApp Printer Bridge

A small Windows tray app that lets **web applications print silently on local printers** -
shipping labels, invoices, receipts - without the browser's print dialog. The web app sends the
document (PDF, image or raw printer commands) over a local WebSocket together with an
**alias** such as `LABEL`; the bridge prints it on the Windows printer assigned to that alias.

* Single `printer-bridge.exe` + `pdfium.dll` (~5.5 MB zipped). No Java, no VC++ runtime, no
  admin rights needed.
* Native builds for **x64** and **ARM64** Windows 10/11.
* Settings window: map aliases to installed printers, choose the network address, token, ...
* Built-in per-user installation, autostart and uninstall - no separate setup program.

## Background

This is a Rust port of the printer part of
[**WebApp Hardware Bridge**](https://github.com/imTigger/webapp-hardware-bridge) 1.0.1 by
imTigger (Java, MIT License, Copyright (c) 2017 imTigger). The app icon is derived from its
icon. The WebSocket protocol and the `config.json` format are kept, so web apps written for the
Java bridge work unchanged and an existing `config.json` can be reused.

Why a port: no Java runtime to ship and update, small download, fast start, native ARM64 support,
a native settings window instead of a local web UI, and clearer error messages (e.g. when the
configured network address no longer exists on the PC).

Only printing is ported. See [Differences from the original](#differences-from-the-original).

## Installation

1. Download `printer-bridge-<version>-win-x64.zip` (Intel/AMD PCs) or
   `printer-bridge-<version>-win-arm64.zip` (Snapdragon and other ARM PCs) from the
   releases page, or [build it yourself](#building-from-source).
2. Extract the **whole** zip (`printer-bridge.exe` and `pdfium.dll` must stay together).
3. Start `printer-bridge.exe`. A tray icon appears.
4. Right-click the tray icon -> **Install for this user...**. This
   * copies the program to `%LOCALAPPDATA%\Programs\WebApp Printer Bridge\`,
   * adds a Start menu entry and an entry in *Settings -> Apps -> Installed apps*,
   * starts the bridge automatically when you sign in,
   * takes over `config.json` if the extracted folder contains one,

   then starts the installed copy. No administrator rights are needed; the installation is
   for the current Windows user only.
5. Left-click the tray icon to open **Settings**, assign printers to your aliases, **Save**.

The release exe is not code-signed, so Windows SmartScreen may warn on first start
("More info" -> "Run anyway").

**Portable use:** skip step 4 and run the exe from any folder. *Start with Windows* in the
settings works for portable copies too.

**Update:** extract the new zip and start its `printer-bridge.exe` while the installed bridge is
running. It offers to replace the installed copy; settings are kept.

**Uninstall:** *Settings -> Apps -> Installed apps -> WebApp Printer Bridge -> Uninstall*.
You can keep the settings for a later installation or remove everything.

**Unattended installation:** `printer-bridge.exe --install` (run from the extracted folder)
installs without questions and starts the installed copy.

**Migrating from the Java bridge:** exit it (tray -> Exit), extract the zip into the Java
bridge's installation folder (the one with its `config.json`), start `printer-bridge.exe` and
choose *Install for this user...*. Then remove the Java bridge's autostart shortcut
(`shell:startup`) and uninstall the Java bridge.

## Usage

### Tray menu

| item | |
|---|---|
| **Settings...** | settings window (also: left-click the tray icon) |
| Open config.json | open the file in the default editor (Notepad if `.json` is unassociated) |
| Install for this user... | only shown for copies that are not installed (see above) |
| App Directory / Log Directory | open the folder in Explorer |
| Restart | re-read `config.json` and restart the server |
| Exit | stop and quit |

### Settings window

* **Printer aliases** - the names web apps send as `type`. Add/remove aliases and pick the
  printer from a drop-down of installed printers. Per alias:
  * *Auto rotate* - PDF pages that are wider than tall are printed in landscape.
  * *Use the whole sheet* - fit into the whole paper instead of the driver's printable area.
  * *Force DPI* - rasterise PDFs at this resolution (0 = vector output, recommended).

  Aliases without a printer and printers that are no longer installed are flagged.
* **Unknown aliases** - add them to the list automatically (then assign a printer here),
  and/or print them on the Windows default printer.
* **Server and app** - bind address (drop-down: all adapters, this PC only, or one of this
  PC's addresses; an address that does not exist on this PC is flagged), port, token,
  notifications, *Write log file* (`log\client.log`, for troubleshooting) and
  *Start with Windows*.

**Save** validates the input, writes `config.json` (other keys are kept) and restarts the
server immediately; a notification confirms the new address.

`config.json`, `log\` and `downloads\` live next to the exe.

## For web developers

Connect to `ws://<host>:<port>/printer` (default `ws://127.0.0.1:12212/printer`) and send
one JSON message per document:

```js
const printer = new WebSocket("ws://127.0.0.1:12212/printer");
printer.onmessage = (event) => console.log(JSON.parse(event.data)); // result per job
printer.onopen = () => {
  printer.send(JSON.stringify({ type: "LABEL", url: "https://example.com/label.pdf", id: "order-4711" }));
};
```

Message fields:

```json
{ "type": "LABEL", "url": "https://host/label.pdf", "id": "job-1", "qty": 1 }
{ "type": "LABEL", "url": "label.pdf", "file_content": "<base64 PDF/PNG/JPG/GIF>" }
{ "type": "LABEL", "raw_content": "<base64 ESC/POS, ZPL, ...>" }
{ "type": "LABEL", "url": "...", "extras": [ { "text": "Hi", "x": 10, "y": 20, "size": 12, "bold": true } ] }
```

* `type` is the alias; the user assigns it to a printer in the settings.
* `raw_content` is sent to the printer unchanged (spooler datatype RAW).
* Otherwise the document type is taken from the file name in `url` (`.pdf`,
  `.jpg/.jpeg/.png/.gif`, case-sensitive). With `file_content` the `url` only provides that
  name; without it the bridge downloads `url`.
* `qty` = copies, `extras` = text printed on every page (mm from the top-left of the
  imageable area to the baseline, size in pt).
* Every result is sent to **all** connected `/printer` clients, in order per connection:
  `{"success":true,"message":"Success","id":"job-1","printerName":"..."}`, or
  `"success":false` with the error (e.g. `No matched printer: LABEL`, `HTTP Status Code: 404`).
* With a token configured, append `?token=...` to the URL (wrong or missing token: the
  connection is closed with code 1003 "Invalid token").

HTTP (CORS open): `GET /system/printers.json` (installed printers) and
`GET /system/version.json`; with a token via `Authorization: Bearer <token>`.

The bridge speaks plain `ws://` (no TLS). Browsers allow pages served over HTTPS to connect to
`ws://127.0.0.1` / `ws://localhost`, but not to other `ws://` addresses; some browsers ask the
user once to allow access to apps on the local device. Use the loopback address when the web
app is served over HTTPS.

See original developer's repository for [**demo examples**](https://github.com/imTigger/webapp-hardware-bridge/tree/master/demo).


## config.json

| key | meaning |
|---|---|
| `printer.mappings[]` | alias `type` -> Windows printer `name` (case-insensitive) + `autoRotate`, `resetImageableArea`, `forceDPI` |
| `printer.autoAddUnknownType` | append unknown aliases (printer empty) |
| `printer.fallbackToDefault` | print unknown aliases on the default printer |
| `printer.enabled` | `false` disables `/printer` |
| `server.bind`, `server.port` | listen address; must exist on this PC (`0.0.0.0` = all, `127.0.0.1` = local only) |
| `server.authentication.enabled`, `.token` | require the token |
| `gui.notification.enabled` | notifications for print jobs and print errors |
| `log.enabled` | write `log\client.log` (default `true`) |
| `downloader.timeout` (s), `.path`, `.ignoreTLSCertificateError` | URL downloads (file only) |

Keys of the Java bridge that are not used here (`server.address`, `server.tls`, `serial`) are
ignored and kept. A UTF-8 BOM is accepted. An invalid file is reported and never
overwritten; fix it and choose *Restart*. *Start with Windows* is stored in the registry
(`HKCU\Software\Microsoft\Windows\CurrentVersion\Run`), not in `config.json`.

## PDF printing

PDFs are rendered with [PDFium](https://pdfium.googlesource.com/pdfium/) (Chrome's PDF
engine, `pdfium.dll` next to the exe) straight into the printer's device context: the driver
receives vector output (text, paths, images) and rasterises it at its own resolution. Text,
lines and barcodes stay sharp, also on 1-bit label printers, and spool files are small.
Annotations (form field appearances, stamps) are printed. Pages are scaled down to fit when
they are larger than the paper, never enlarged, and centered.

With *Force DPI* the page is instead rasterised at that resolution and scaled onto the page.

Known producer defects are repaired in memory before rendering (`src/printing/pdf_repair.rs`,
only affected files are touched), e.g. an odd-length `CIDToGIDMap` written by dompdf 3.0.0 +
CPDF, which makes some renderers drop all text set in that font.

If `pdfium.dll` is missing or cannot be loaded, the bridge still runs: PDF jobs fail with
`PDF printing is unavailable: ...`, images and RAW jobs keep working. A `pdfium.dll` of the
wrong architecture is refused at startup.

## Troubleshooting

* **"Unable to start server: Cannot listen on 192.168.x.y:12212 ..."** - the bind address is
  not (or no longer) an address of this PC, e.g. after a network change. Choose another bind
  address in Settings (`0.0.0.0` survives IP changes).
* **Nothing prints / wrong printer** - Settings: is the alias listed and a printer assigned?
  Turn on *Write log file*; `log\client.log` shows every job
  (`Sending print job type: X to printer: Y`) and errors.
* **Web app cannot connect from another PC** - bind `0.0.0.0` or the LAN address and allow the
  port in the Windows firewall.
* **Raw jobs fail on a printer** - RAW needs a classic (v3) driver; XPS-only (v4) drivers
  reject raw data.
* **"PDF printing is unavailable"** / **"pdfium.dll is built for ..."** - put the `pdfium.dll`
  from the same zip next to `printer-bridge.exe`.

## Differences from the original

| | Java bridge | WebApp Printer Bridge |
|---|---|---|
| Runtime | bundled Java runtime | single native exe + `pdfium.dll` |
| Platforms | Java (Windows installer provided) | Windows only, x64 and ARM64 |
| Setup | NSIS setup program with desktop and Start menu shortcuts | no separate setup program: the exe installs itself per user (Start menu entry, "Installed apps" entry, uninstall); no desktop shortcut |
| Autostart | shortcut in the Startup folder, created by the setup | *Start with Windows* in the settings (registry Run entry; switched on by the installation, works for portable copies too) |
| Configuration | local web UI, `GET/PUT /config.json`, `/system/restart.json` | native settings window; HTTP API is read-only |
| Serial ports, TLS/WSS | supported | not supported |
| PDF engine | PDFBox | PDFium (both send vector output to the driver) |
| Log file | always | optional (`log.enabled`) |
| Errors | partly only in the log | notifications with the actual cause |

Unchanged on purpose: images are stretched over the imageable area, copies are uncollated,
`bold` is applied when the key is present, downloaded files are kept in `downloads\` (deleted
only when the job fails).

## Building from source

Requirements:

* Windows 10/11 (x64 or ARM64).
* [Rust](https://rustup.rs) (stable, MSVC toolchain) with both targets:
  `rustup target add x86_64-pc-windows-msvc aarch64-pc-windows-msvc`
* Visual Studio 2022 Build Tools (or Visual Studio) with *Desktop development with C++*: MSVC
  build tools for x64/x86 **and** ARM64, and a Windows SDK (provides `rc.exe` for the icon and
  version resource).
* PowerShell (Windows PowerShell 5.1 is fine).

Build and test:

```
git clone https://github.com/joffs/webapp-printer-bridge.git
cd webapp-printer-bridge
powershell -ExecutionPolicy Bypass -File scripts\fetch-pdfium.ps1
cargo test
cargo build --release
```

`scripts\fetch-pdfium.ps1` downloads the pinned PDFium release (tag `chromium/7881` of
[bblanchon/pdfium-binaries](https://github.com/bblanchon/pdfium-binaries), SHA-256 checked)
into `vendor\pdfium\<arch>\`. `build.rs` copies the matching `pdfium.dll` next to the built
exe, examples and tests, so `target\release\printer-bridge.exe` runs as is. The C runtime is
linked statically (`.cargo\config.toml`).

Release packages for both architectures:

```
powershell -ExecutionPolicy Bypass -File scripts\build-release.ps1 [-Arch x64,arm64]
```

creates `dist\printer-bridge-<version>-win-<x64|arm64>\` and a zip of it: `printer-bridge.exe`,
the matching `pdfium.dll`, `README.md`, `LICENSE` and `licenses\pdfium\` (PDFium is
BSD-3-Clause; its third-party licenses are included). The script checks that exe and DLL
have the package's architecture.

Dev tools (`examples/`):

```
cargo run --example make_test_pdf -- test.pdf [--barcode]   # test pages: A4, landscape, label, /Rotate; barcode label
cargo run --example print_file -- "Microsoft Print to PDF" test.pdf --out out.pdf [--rotate] [--dpi N] [--copies N] [--annotate] [--raw]
cargo run --example render_pdf -- in.pdf page 400           # PDF pages -> PNG with PDFium
```

`print_file --out` spools to a file, so "Microsoft Print to PDF" works without the save dialog
(the file appears once the spooler has finished).

### Source layout

| file | |
|---|---|
| `src/main.rs` | startup, tray commands, install/update/uninstall flow, settings save -> restart |
| `src/server.rs` | HTTP/WebSocket server (axum), token check, result broadcast |
| `src/bridge.rs` | job pipeline: alias -> printer lookup, download/decode, dispatch, results |
| `src/printing/` | `winspool.rs` printers + RAW, `gdi.rs` GDI jobs/pages/text, `pdfium.rs` PDFium binding, `pdf.rs` documents/rendering, `pdf_repair.rs` |
| `src/install.rs` | per-user install/uninstall, autostart, Start menu shortcut, registry |
| `src/settings.rs` | settings window (Win32) |
| `src/tray.rs` | tray icon, menu, notifications |
| `src/config.rs` | `config.json` load/save (compatible with the Java bridge) |
| `src/protocol.rs` | message types, lenient parsing (as the Java bridge) |
| `src/netinfo.rs`, `src/logging.rs`, `src/notify.rs` | local IPs, switchable rolling log (10 MB x 5), notification hook |

## License

MIT, see [LICENSE](LICENSE). PDFium is BSD-3-Clause (license files are included in the
release zips).
