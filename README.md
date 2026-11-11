# LTO Catalog for RustySalamander

A RustySalamander plugin for LTFS tapes in LTO tape libraries and standalone LTO drives. It is written for an IBM TS3100 (SAS) with two LTO-5 drives, and works with any SCSI medium changer, several of them, and drives without one.

- **Browse every tape offline.** A catalog (SQLite) keeps what each tape's LTFS volume holds. Plugins → *Open tape catalog* shows the tapes by barcode, and you can browse them without loading any.
- **The robot loads tapes by itself.** Copying from a tape (F5, F3, dragging out) or onto one has the library load it into a free drive. With two drives, two tapes are used at once. Files are read in the order they lie on the tape.
- **Standalone drives too.** A drive outside any library is used for tapes you put in by hand; the plugin recognises which tape it is. See [Libraries and standalone drives](#libraries-and-standalone-drives).
- **Missing tapes are asked for.** When a job needs a tape that no library or drive has, a window says where it can go (a library's mail slot, a standalone drive) and what the drives hold: *Retry*, *Skip this tape* or *Cancel*.
- **Native LTFS, no driver.** The plugin reads and writes LTFS 2.4 itself through Windows' SCSI pass-through: labels, indexes, appending files, formatting. IBM's or HPE's LTFS software isn't needed.

It is a separate project from RustySalamander and isn't one of its official plugins.

## Contents

| Folder | What |
|---|---|
| `crates/lto-scsi` | SCSI commands for tape drives and changers, Windows pass-through, the `Library` that loads tapes on demand, the `Site` of every library and standalone drive |
| `crates/ltfs` | the LTFS format: labels, the index (XML), reading, appending, formatting |
| `crates/lto-catalog` | the offline catalog (SQLite, built in) |
| `crates/lto-archive` | jobs both the plugin and `ltocat` use: inventory, import, read, write, format |
| `crates/lto-sim` | simulated libraries and standalone drives answering the same SCSI commands, for tests and for trying the plugin without hardware |
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
ltocat devices          the drives (with serial numbers) and the changers Windows shows
ltocat selftest         opens the libraries, matches their drives to \\.\TapeN, lists standalone drives
ltocat inventory        where every tape is
ltocat import <barcode> loads one tape and reads its index (if it holds LTFS)
```

If `devices` doesn't find the changer, give it explicitly: `--changer scsi:<port>:<bus>:<target>:1` (several: comma-separated). `devices` prints each drive's `scsi:` address, and the changer is usually the same address with LUN 1. Give the same value in the plugin under Plugins → *Library settings…*. Without a changer found, the drives are used as standalone drives.

Then, **with a scratch tape whose contents don't matter**:

```text
ltocat format <barcode> --name Test        erases it; asks for the barcode again
ltocat put C:\some\folder <barcode>\test   writes a folder
ltocat get <barcode>\test C:\restore       reads it back; compare with the original
```

If possible, check the tape with another LTFS program (IBM Spectrum Archive / `ltfs` on Linux) before trusting it with real data.

Then in RustySalamander: Plugins → *Open tape catalog*, *Import tapes…*, and copy as usual.

## Libraries and standalone drives

Every changer and tape drive the computer has is used. Each changer claims the drives that report their serial numbers to it; the drives no changer claims are **standalone**, and tapes are put into them by hand. Any mix works: one library, several, standalone drives only, or both.

- **A job finds its tape** in a drive that already holds it (in a library or standalone), else in a library, whose robot loads it. The same barcode in two libraries is noted in the status; the first library's tape is used.
- **Which tape is in a standalone drive** (it has no barcode reader): the barcode in the cartridge memory (libraries, *Format a tape…* and most LTFS software write it), else the LTFS volume's UUID if the catalog has seen it, else the volume serial of the tape's VOL1 label (the barcode's first six characters), matched to a barcode in the catalog. What was found is remembered per cartridge (by its serial number in the cartridge memory), so a tape is read once, not at every job. A tape that can't be told apart is shown as *a tape that can't be identified* and isn't used.
- **A tape that isn't there** is asked for: the window names the libraries' mail slots and the standalone drives, and what each drive holds. When every standalone drive holds another tape, the one used least recently is ejected first, to make room; a tape ejected this way isn't loaded again until someone takes it out or puts a tape in.
- **While a job uses a standalone drive**, its eject button is locked.
- **Formatting a new tape in a standalone drive:** a tape with no LTFS volume and no barcode in its memory is taken as the barcode you typed. A tape that is recognised as another one is never formatted in its place. The format writes the barcode into the cartridge memory, so the tape is recognised from then on.
- **Ejecting** (Plugins → *Eject a tape…*, `ltocat eject`): a library moves the tape to its mail slot, a standalone drive ejects it.
- **Libraries or drives connected while the program runs** are found by *Inventory the libraries and drives* or *Library status* (when no job is running).

## Trying it without a library

```powershell
$env:LTOCATALOG_SIM = "1"              # a simulated TS3100 with 6 tapes, kept in sim-library.bin next to the plugin
$env:LTOCATALOG_SIM_LIBRARIES = "2"    # optional: more libraries (sim-library-2.bin…, tapes LT2001L5…); 0 for none
$env:LTOCATALOG_SIM_DRIVES = "1"       # optional: standalone drives (sim-drive-1.bin…)
$env:RUSTYSALAMANDER_PLUGINS = "D:\Projects\RustySalamander_LTOCatalog\target\release\dist\plugins"
D:\Projects\RustySalamander\target\release\rustysalamander.exe
```

`ltocat --sim <file> --sim-libraries <n> --sim-drives <n> --catalog <file> …` does the same for the command line. The simulated tapes hold 256 MiB each. Moving tapes by hand, as a person would, is `ltocat … sim-move <barcode> drive1|mail2|shelf` (a new blank tape when the barcode is nowhere yet); the shelf keeps tapes that are in no library or drive.

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
- **Matching drives to the changer's drive elements** by serial number. If the changer doesn't report serials, drives are matched in order (only with one changer and no other drives), and `selftest` says so.
- **Standalone drives:** loading and ejecting by hand, the eject lock, and reading the cartridge serial number and barcode from the cartridge memory.

## License

GPL-2.0-or-later, like RustySalamander. SQLite (built in) is in the public domain.
