# LTO Catalog for RustySalamander

A RustySalamander plugin for LTFS tapes in LTO tape libraries and standalone LTO drives. It is written for an IBM TS3100 (SAS) with two LTO-5 drives, and works with any SCSI medium changer, several of them, and drives without one.

- **Back up and archive** folders and files onto tapes, checked after writing; an archive deletes what is safely on the tapes. See [Backing up and archiving](#backing-up-and-archiving).
- **Browse every tape offline.** A catalog (SQLite) keeps what each tape's LTFS volume holds. Plugins → *Open tape catalog* shows the tapes by barcode, and you can browse them without loading any.
- **The robot loads tapes by itself.** Copying from a tape (F5, F3, dragging out) or onto one has the library load it into a free drive. With two drives, two tapes are used at once. Files are read in the order they lie on the tape.
- **Standalone drives too.** A drive outside any library is used for tapes you put in by hand; the plugin recognises which tape it is. See [Libraries and standalone drives](#libraries-and-standalone-drives).
- **Search the catalog** by a part of a name or a mask (`*.mkv`), on every tape or one; copy what is found straight out of the results.
- **Format several tapes in one job,** ticked from the tapes in the libraries and drives.
- **Right-click on tapes** for the plugin's commands (RustySalamander 0.9.9-beta or later).
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
| `crates/lto-sim` | simulated libraries and standalone drives answering the same SCSI commands, for tests and for trying the plugin without hardware ([SIMULATOR.md](SIMULATOR.md)) |
| `plugin` | the RustySalamander plugin (`ltocatalog.spl`) |
| `tools/ltocat` | the command line, for checking the library and for work without the program |

## Building

The project needs RustySalamander's repository next to it (`..\RustySalamander`), because it uses the plugin interface from there. The toolchain is the same as RustySalamander's: x86_64-pc-windows-gnu with WinLibs MinGW.

```powershell
. .\env.ps1
cargo test --workspace      # everything, on the simulated library (SIMULATOR.md)
.\package.ps1               # release build → target\release\dist
```

## Installing

1. Unzip `ltocatalog-<version>.zip` into the RustySalamander folder (the one with `rustysalamander.exe`): `plugins\ltocatalog` lands among the other plugins, with `ltocat.exe` and this README, the CHANGELOG and SIMULATOR.md in it, where it finds the plugin's catalog and settings by itself. RustySalamander's update check (Help → Check for updates) keeps both up to date. To type just `ltocat`, add `<RustySalamander folder>\plugins\ltocatalog` to PATH. An `ltocat.exe` next to `rustysalamander.exe`, from a release before 0.4.1, is no longer updated: delete it.
2. **Run RustySalamander as administrator.** SCSI pass-through needs it.
3. The tape drives need their Windows driver, so that `\\.\Tape0` and `\\.\Tape1` exist; IBM's tape driver or Windows' own LTO driver both work. The changer needs no driver. Without one, the plugin reaches it through the SAS adapter (the TS3100's changer is LUN 1 of the first drive).

## First use on the library (step by step)

Check the hardware with `ltocat` first, in an administrator prompt. Until step 4, nothing is moved or written. In `plugins\ltocatalog` (or next to `rustysalamander.exe`), `ltocat` uses the plugin's catalog and library settings; anywhere else, tell it where the program is with `--program <RustySalamander folder>` (or name a catalog with `--catalog <file>`):

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
ltocat put <folder> <barcode>\test          writes a folder
ltocat get <barcode>\test <empty folder>    reads it back; compare with the original
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

A simulated TS3100 with six tapes (and, if you like, more libraries and standalone drives) stands in for the hardware: set `LTOCATALOG_SIM=1` before starting RustySalamander, or give `ltocat` `--sim <file>`. See **[SIMULATOR.md](SIMULATOR.md)**: what is simulated, where its files are, moving tapes by hand (`sim-move`), damaging them (`sim-damage`), and what to try.

## Where things are kept

Wherever RustySalamander keeps its settings, so the plugin is as portable as the program:

- **Portable (the default):** `catalog.sqlite` and `settings` in the plugin's folder, `plugins\ltocatalog`. Nothing is written outside the program's folder.
- **Multi-user mode** (RustySalamander's Settings → General → Settings location; `multiuser.toml` next to the program): in each user's `%APPDATA%\RustySalamander\plugins\ltocatalog`. Switching moves both files there.
- *Library settings…* can put the catalog elsewhere, such as a backed-up share; `ltocat --program` then uses that one too.
- `ltocat` finds them through the program's folder (its own folder, or `--program`), the same way the plugin does; `--catalog`, `--changer` and `--drives` override what the plugin's settings say.

## How it works with the tapes

- **Reading.** Tapes already in a drive are read first. Each tape's files are read in their order on the tape, so it isn't wound back and forth.
- **Writing.** Files are appended to the data partition in 512 KiB records. While the tape writes, the next files are read from disk into a buffer (*Library settings…*: *While writing onto tapes, read ahead from disk*, 256 MB by default, 0 to turn it off), so a slow disk (a network share, many small files) doesn't stop the tape; every 5 seconds the detailed log says how full it is and which side waited for the other. After a copy, a new index is written to the data partition, then over the index partition's index, and the volume coherency attribute (MAM 0x080C) is updated, as other LTFS programs do. A cancelled or failed copy still ends with a consistent index; the file being written when it stopped isn't in it.
- **Deleting and renaming** change only the index. LTFS never frees space on a tape.
- **Before any change**, the tape's newest index is compared with the catalog. If another program changed the tape, the catalog is updated from the tape and the change isn't made, so you can look first.
- **Tapes that are refused for writing:** write-protected tapes, locked volumes, and volumes where another program stored files in the index partition. Reading them works.
- **Names** that Windows can't have (`:` `?` `*`, a trailing dot…) are shown with those characters in the Unicode private-use area, and written back unchanged.

## Backing up and archiving

*Plugins → Back up or archive…* opens a dialog in sections:

- **Do:** *Back up* (the files stay where they are) or *Archive* (the files will be deleted from their original location once they are on the tapes).
- **Name:** the folder on the tapes, *Backup* or *Archive* unless you type another (`\` makes folders in it), with the date and time appended unless you untick it: `Backup 2026-11-16 10.00`. Under it, a folder backed up alone keeps its name (`…\Documents\…`), items of one folder go into that folder's name, items from several folders right into it. The dialog shows the folder it will be. Without the date, a later backup of the same name goes into the same folder and replaces its files of the same name.
- **Options:**
  - all files, or only those whose Archive attribute is set (changed since the last backup that cleared it); folders without it are made on the tape only where a changed file goes;
  - *Clear the Archive attribute of the files backed up* (the default): cleared once a file is on a tape, so the next "only Archive" backup takes only what changed since (incremental); unticked, it stays (differential). A file that changed while it was written keeps its attribute;
  - *Check the data right after it is written:* before a tape is unloaded, its files are read back and compared byte for byte with the files on disk.
- **Delete** (only for Archive): *without asking, as soon as the files are on the tapes*; *without asking, once the data is checked* (this turns the check on); or *ask before deleting, once everything is written*. With the check, only the files read back the same are deleted; without it, every file a tape's index holds. Deleted for good, not to the Recycle Bin, and so are the folders left empty; a file changed since it was written or checked stays, and so do the folders the items were in. Nothing is deleted after Cancel.
- **Files and folders:** the items selected in the file panel (the active one, else the other), or its folder when nothing is selected, ticked; **Browse…** adds a file or folder from anywhere.
- **Tapes:** the LTFS tapes in the libraries and drives, with their free space and notes. The ones ticked are filled in barcode order. A file goes onto a tape whole; one that doesn't fit on the rest of a tape goes onto the next. When the ticked tapes are full, a new tape is asked for in the mail slot:
  - an empty LTFS tape is used;
  - a tape with files is refused;
  - a blank tape, or one in another format, is formatted (named by its barcode) only after you confirm.

While it runs, the status bar shows the files, the data and the average speed, and *Cancel* stops it: what was written stays on the tapes, with its index.

- **The report** comes at the end whatever happened, in a window whose result is green when everything was backed up (and checked, and deleted) and red when not: what was found, each tape with its files and data, the tapes that failed, the check and what was deleted, and how many files had each kind of problem. *Save full report…* saves the whole of it with every file name.
- **A tape that fails** during a backup (it can't be read or written) is noted as damaged, and you are asked whether to go on with another tape. What the failed tape's last index holds is safe there; everything after it is written onto the next tape.
- **The index along the way:** every 100 GB the index is written, so a failure (power, a crash) loses at most that much. By default it goes only to the end of the data, as IBM LTFS does while a tape is mounted: a stop of a few seconds. *Library settings…* can have it written into the index partition too, as at the end of each tape; the tape then moves there and back each time, a minute or two. Either way, the index at the end of each tape is written into both partitions.

From the command line: `ltocat backup <folder> [<barcode>...] [--archive-only] [--keep-archive] [--index data|both] [--check] [--name <name>] [--no-date] [--read-ahead <MB>]`, and `ltocat archive <folder> …` (checks, then deletes after asking; `--yes` doesn't ask, `--keep-source` only checks).

## Damaged tapes

LTO drives retry and correct errors themselves; the plugin only sees what a drive gave up on (a *medium error*), and that goes into the detailed log with the drive's sense codes.

- **One copy of the index can't be read.** LTFS keeps the label and the index in both partitions. When one partition's can't be read, the other's is used and a warning says so before the job goes on (in a modal window from RustySalamander 0.9.10; a separate window before). The tape is then used read-only. If the copy read is older than what the catalog lists (the newest index was at the end of the data), the catalog keeps the newer listing, and the warning says which files can't be read from the tape.
- **Check tapes** (*Plugins → Check tapes…*, `ltocat verify <barcode>...`) reads every file of the tapes ticked, in their order on the tape, and lists the files that can't be read; the others are read on. Nothing is written, and the data read is thrown away. A full LTO-5 tape takes about 2 hours.
- **What the drives report (TapeAlert).** After each use of a drive, its TapeAlert flags are read: "the tape is damaged", "the tape is nearing the end of its life", "the drive needs cleaning", hardware faults. They are shown with the command's result (after a copy, in a window of their own).
- **Damaged in the catalog.** Read or write failures, an index that can't be read, files a check couldn't read, and what TapeAlert says about a tape are noted against the tape: the tape lists in the forms and *Library status* show "Damaged: …" (`ltocat tapes`: DAMAGED). A check that reads the whole tape without a fault removes the note.
- **On the simulator,** `sim-damage` damages a tape, to see all this without a damaged one: see [SIMULATOR.md](SIMULATOR.md#damaging-a-tape-sim-damage).

## The detailed log

`ltocatalog.log`, next to the catalog (see above), records what the plugin does with the tapes: every SCSI command (data records summed up), what each partition of a tape holds when it is mounted, every index written, the catalog compared with the tape, the loads, the backups file by file, the commands, questions and errors. It is on by default and can be turned off in *Library settings…*. At 64 MB it is renamed `ltocatalog.1.log` (four older files are kept). `ltocat` writes `ltocat.log` in the same folder (or `--log <file>`). When something goes wrong on a tape, that file says what happened; it holds file and folder names, so look through it before sending it to anyone.

## Safety

- Only a device that says it is a tape drive gets tape commands, and only a medium changer changer commands; a disk named by mistake in *Library settings…* is left out (*Library status* says so).
- The catalog is always a file in a folder that exists, never a device (`NUL`, `\\.\PhysicalDrive0`) or a folder.
- Barcodes are letters A–Z and digits; volume names and device names have no control characters.

## Tested hardware

Tested with an **IBM TS3100** tape library (SAS) with LTO-5 drives. The automated tests run against the simulator, which follows the SCSI standards (SSC-4, SMC-3).

Not tested on real hardware yet:

- **Standalone drives:** loading and ejecting by hand, the eject lock, and reading the cartridge serial number and barcode from the cartridge memory.
- **Several libraries** connected at once.

## License

GPL-2.0-or-later, like RustySalamander. SQLite (built in) is in the public domain.
