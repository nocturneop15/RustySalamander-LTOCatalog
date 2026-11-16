# The LTO simulator

The plugin and `ltocat` can work on **simulated tape libraries and drives** instead of hardware. The simulator answers the same SCSI commands a real IBM TS3100 and its LTO-5 drives do (SSC-4 for the drives, SMC-3 for the changer), so everything the plugin does — inventory, loading, formatting, copying, backups and archives, checking, damaged tapes — can be tried without a library, and the automated tests run on it.

Nothing about the simulator touches real devices: while it is on, the plugin doesn't open any.

## What is simulated

| | |
|---|---|
| **Library** | a changer with 24 slots, 1 mail slot and 2 drives, holding six tapes in the first slots: `LTO001L5` … `LTO006L5`. More libraries have their own six tapes: `LT2001L5` … in the second, `LT3001L5` … in the third. |
| **Standalone drives** | drives outside any library, empty at first; tapes are put into them by hand (`sim-move`). |
| **The shelf** | where tapes taken out of every library and drive wait (up to 200). |
| **Tapes** | LTO-5 cartridges of **256 MiB** each (small, so a full tape and a backup over several tapes are quick to try), with two partitions once formatted, and cartridge memory (MAM: barcode, serial number, volume coherency). New tapes are blank. |
| **Drives** | TapeAlert flags, early warning near the end of a tape, buffered writes, medium errors on damaged records. |

Everything is kept in files, so tapes keep what was written onto them from one run to the next:

| File | What |
|---|---|
| `sim-library.bin` | the first library, its drives and its tapes |
| `sim-library-2.bin`, `sim-library-3.bin` … | more libraries |
| `sim-drive-1.bin`, `sim-drive-2.bin` … | standalone drives |
| `sim-shelf.bin` | the shelf |

All of them are in one folder: the first library's. Delete them to start again with new tapes; the catalog (`catalog.sqlite`) remembers the old ones until you *Forget* them or delete it too.

## In RustySalamander

In a PowerShell prompt in the RustySalamander folder (the one with `rustysalamander.exe`), set the variables, then start the program from the same prompt:

```powershell
$env:LTOCATALOG_SIM = "1"              # on: the files in the plugin's data folder
$env:LTOCATALOG_SIM_LIBRARIES = "2"    # optional: libraries (default 1; 0: none)
$env:LTOCATALOG_SIM_DRIVES = "1"       # optional: standalone drives (default 0)
.\rustysalamander.exe
```

- `LTOCATALOG_SIM = "1"` keeps the files in the plugin's data folder (`plugins\ltocatalog`, or `%APPDATA%\RustySalamander\plugins\ltocatalog` in multi-user mode), next to its catalog. Any other value is the path of the first library's file, e.g. `sim\sim-library.bin` (relative paths are from the folder the program was started in, here the RustySalamander folder); the others go next to it.
- The program doesn't need to run as administrator for the simulator.
- *Library settings…* (changer, drives) don't matter while it is on; the catalog, the index and read-ahead settings and the log do.
- To try a plugin you built rather than an installed one, point the program at the build's `plugins` folder: `$env:RUSTYSALAMANDER_PLUGINS = "<project folder>\target\release\dist\plugins"`.

Use **another catalog** for the simulator than for the real library, so simulated tapes don't mix with real ones: a separate copy of the program, or *Library settings…* → Catalog file.

## With ltocat

The same simulation from the command line:

```text
ltocat --sim <file> [--sim-libraries <n>] [--sim-drives <n>] --catalog <file> <command> ...
```

`--sim <file>` is the first library's file (the others next to it), as `LTOCATALOG_SIM` with a path. Use the same file and catalog as the plugin to see what it did; from the RustySalamander folder: `plugins\ltocatalog\ltocat --sim plugins\ltocatalog\sim-library.bin --catalog plugins\ltocatalog\catalog.sqlite …`.

Two commands work on the simulator only — what a person does with the hardware:

### Moving tapes by hand: `sim-move`

```text
ltocat --sim <file> sim-move <barcode> drive<N>|mail<N>|shelf
```

- `drive1`, `drive2` … — into a standalone drive (as putting a tape into it);
- `mail1`, `mail2` … — into a library's mail slot (`mail2`: the second library's), from where the library takes it in;
- `shelf` — out of every library and drive.

A barcode that is nowhere yet is a **new blank tape**, made where it is put: the way to give a backup new tapes, or to try formatting. A tape can't be taken out of a drive while a job holds it (its eject button is locked). When the place is full, the tape goes onto the shelf instead and `sim-move` says so.

### Damaging a tape: `sim-damage`

```text
ltocat --sim <file> sim-damage <barcode> a|b [<record>]
```

Makes a record of partition `a` (the index partition) or `b` (the data partition) unreadable: the drive then reports a medium error there, as a worn or damaged tape does. Without `<record>`, the start of the partition's last index is damaged, so the plugin has to use the other partition's copy.

What to try with it:

- `sim-damage <barcode> a` — the index partition's index can't be read: the tape is still read, from the data partition's index, read-only, with a warning.
- `sim-damage <barcode> b <record>` on a record of a file (the detailed log says where each file a backup wrote starts: `backup: "…" 1048576 bytes at b:<record>`) — *Check tapes* finds the file that can't be read and notes the tape as damaged; copying it out fails for that file.
- A damaged tape during a backup — the backup asks whether to go on with another tape.

A damaged record stays damaged; a check of the whole tape keeps finding it. To start again with good tapes, delete the `sim-*.bin` files.

## Trying things out

| To see | Do |
|---|---|
| a backup over several tapes | back up a folder of more than 256 MiB onto one ticked tape; when it is full, a new tape is asked for: `sim-move NEW001L5 mail1`, then *Retry* (a blank tape is formatted after you confirm) |
| an archive | *Back up or archive…* → Archive, with the check after writing; the files are deleted from their place once read back the same |
| a missing tape | `sim-move <barcode> shelf`, then open a file on that tape: the window asks for it |
| a standalone drive | `$env:LTOCATALOG_SIM_DRIVES = "1"`, then `sim-move <barcode> drive1` |
| several libraries | `$env:LTOCATALOG_SIM_LIBRARIES = "2"`: tapes `LT2001L5` … are in the second |
| a damaged tape | `sim-damage` above, then *Check tapes…* |

## What it doesn't do

- **Speed:** the simulator is as fast as the disk its files are on; it doesn't simulate the tape's speed, winding, or a drive that stops when its buffer runs dry. The read-ahead buffer's log lines show disk and "tape" rates, but not real ones.
- **Size:** 256 MiB a tape, not 1.5 TB.
- **Hardware faults** other than unreadable records: no power failures, cleaning cartridges, or a drive going away in the middle of a job.

## In the tests

`cargo test --workspace` runs everything against the simulator (`crates/lto-sim`), in memory, without files. Tests make libraries with `lto_sim::Config` (slots, mail slots, drives, capacity, tapes) and change them directly: put tapes in, damage records (`SimLibrary::damage`, `damage_last_index`), raise TapeAlert flags (`raise_alert`).
