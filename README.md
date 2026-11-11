# LTO Catalog for RustySalamander

A RustySalamander plugin for LTFS tapes in an LTO tape library. It is written for an IBM TS3100 (SAS) with two LTO-5 drives, and works with any SCSI medium changer.

- **Browse every tape offline.** A catalog (SQLite) keeps what each tape's LTFS volume holds. Plugins → *Open tape catalog* shows the tapes by barcode, and you can browse them without loading any.
- **The robot loads tapes by itself.** Copying from a tape (F5, F3, dragging out) or onto one has the library load it into a free drive. With two drives, two tapes are used at once. Files are read in the order they lie on the tape.
- **Missing tapes are asked for.** When a job needs a tape that isn't in the library, a window asks you to put it into the mail slot: *Retry*, *Skip this tape* or *Cancel*.
- **Native LTFS, no driver.** The plugin reads and writes LTFS 2.4 itself through Windows' SCSI pass-through: labels, indexes, appending files, formatting. IBM's or HPE's LTFS software isn't needed.

It is a separate project from RustySalamander and isn't one of its official plugins.

## Contents

| Folder | What |
|---|---|
| `crates/lto-scsi` | SCSI commands for tape drives and changers, Windows pass-through, the `Library` that loads tapes on demand |
| `crates/ltfs` | the LTFS format: labels, the index (XML), reading, appending, formatting |
| `crates/lto-catalog` | the offline catalog (SQLite, built in) |
| `crates/lto-archive` | jobs both the plugin and `ltocat` use: inventory, import, read, write, format |
| `crates/lto-sim` | a simulated library answering the same SCSI commands, for tests and for trying the plugin without hardware |
| `plugin` | the RustySalamander plugin (`ltocatalog.spl`) |
| `tools/ltocat` | the command line, for checking the library and for work without the program |

## Building

The project needs RustySalamander's repository next to it (`..\RustySalamander`), because it uses the plugin interface from there. The toolchain is the same as RustySalamander's: x86_64-pc-windows-gnu with WinLibs MinGW.

```powershell
. .\env.ps1
cargo test --workspace      # everything, on the simulated library
.\package.ps1               # release build → target\release\dist
```

## Installing

1. Unzip `ltocatalog-<version>.zip` into the RustySalamander folder (the one with `rustysalamander.exe`): `plugins\ltocatalog` lands among the other plugins and `ltocat.exe` next to the program, where it finds the plugin's catalog and settings by itself.
2. **Run RustySalamander as administrator.** SCSI pass-through needs it.
3. The tape drives need their Windows driver, so that `\\.\Tape0` and `\\.\Tape1` exist; IBM's tape driver or Windows' own LTO driver both work. The changer needs no driver. Without one, the plugin reaches it through the SAS adapter (the TS3100's changer is LUN 1 of the first drive).

## First use on the library (step by step)

Check the hardware with `ltocat` first, in an administrator prompt. Until step 4, nothing is moved or written. Next to `rustysalamander.exe`, `ltocat` uses the plugin's catalog and library settings; anywhere else, tell it where the program is with `--program <RustySalamander folder>` (or name a catalog with `--catalog <file>`):

```text
ltocat devices          the drives (with serial numbers) and the changer Windows shows
ltocat selftest         opens the library and matches its drives to \\.\TapeN
ltocat inventory        where every tape is
ltocat import <barcode> loads one tape and reads its index (if it holds LTFS)
```

If `devices` doesn't find the changer, give it explicitly: `--changer scsi:<port>:<bus>:<target>:1`. `devices` prints each drive's `scsi:` address, and the changer is usually the same address with LUN 1. Give the same value in the plugin under Plugins → *Library settings…*.

Then, **with a scratch tape whose contents don't matter**:

```text
ltocat format <barcode> --name Test        erases it; asks for the barcode again
ltocat put C:\some\folder <barcode>\test   writes a folder
ltocat get <barcode>\test C:\restore       reads it back; compare with the original
```

If possible, check the tape with another LTFS program (IBM Spectrum Archive / `ltfs` on Linux) before trusting it with real data.

Then in RustySalamander: Plugins → *Open tape catalog*, *Import tapes…*, and copy as usual.

## Trying it without a library

```powershell
$env:LTOCATALOG_SIM = "1"          # a simulated TS3100 with 6 tapes, kept in sim-library.bin next to the plugin
$env:RUSTYSALAMANDER_PLUGINS = "D:\Projects\RustySalamander_LTOCatalog\target\release\dist\plugins"
D:\Projects\RustySalamander\target\release\rustysalamander.exe
```

`ltocat --sim <file> --catalog <file> …` does the same for the command line. The simulated tapes hold 256 MiB each.

## Where things are kept

Wherever RustySalamander keeps its settings, so the plugin is as portable as the program:

- **Portable (the default):** `catalog.sqlite` and `settings` in the plugin's folder, `plugins\ltocatalog`. Nothing is written outside the program's folder.
- **Multi-user mode** (RustySalamander's Settings → General → Settings location; `multiuser.toml` next to the program): in each user's `%APPDATA%\RustySalamander\plugins\ltocatalog`. Switching moves both files there.
- *Library settings…* can put the catalog elsewhere, such as a backed-up share; `ltocat --program` then uses that one too.
- `ltocat` finds them through the program's folder (its own folder, or `--program`), the same way the plugin does; `--catalog`, `--changer` and `--drives` override what the plugin's settings say.

## How it works with the tapes

- **Reading.** Tapes already in a drive are read first. Each tape's files are read in their order on the tape, so it isn't wound back and forth.
- **Writing.** Files are appended to the data partition in 512 KiB records. After a copy, a new index is written to the data partition, then over the index partition's index, and the volume coherency attribute (MAM 0x080C) is updated, as other LTFS programs do. A cancelled or failed copy still ends with a consistent index; the file being written when it stopped isn't in it.
- **Deleting and renaming** change only the index. LTFS never frees space on a tape.
- **Before any change**, the tape's newest index is compared with the catalog. If another program changed the tape, the catalog is updated from the tape and the change isn't made, so you can look first.
- **Tapes that are refused for writing:** write-protected tapes, locked volumes, and volumes where another program stored files in the index partition. Reading them works.
- **Names** that Windows can't have (`:` `?` `*`, a trailing dot…) are shown with those characters in the Unicode private-use area, and written back unchanged.

## Not tested yet on real hardware

Everything is tested against the simulator, which follows the SCSI standards (SSC-4, SMC-3). The parts most likely to need adjusting on the real TS3100 / LTO-5:

- **Partitioning in `format`** (MODE SELECT of the medium partition page, then FORMAT MEDIUM). Drives differ in what they accept here.
- **Reaching the changer through the adapter** when it has no driver.
- **Matching drives to the changer's drive elements** by serial number. If the changer doesn't report serials, drives are matched in order, and `selftest` says so.

## License

GPL-2.0-or-later, like RustySalamander. SQLite (built in) is in the public domain.
