# Changelog

## 0.4.2-alpha (2026-11-16)

- **The backup report fits on the screen:** it opens in a window of its own (modal from RustySalamander 0.9.10) with the result on top, **in green when everything was backed up and in red when not** (the backup stopped, was cancelled, or files were left out or couldn't be read). Below it: what was found (folders, files, size), each tape written with its files and data, the tapes that failed with why, and how many files were left out, couldn't be read or kept their Archive attribute, without their names. Before, the report listed the names of up to 20 files of each kind and could run far off the bottom of the screen. A tape that failed while the backup went on with another one is listed but doesn't make the result red: everything is on the tapes.
- **Save full report…** in that window saves the whole report as a text file, where you choose: the folder backed up, the folder on the tapes, the options, the result, every tape, and the name of every file that was left out, couldn't be read or kept its Archive attribute. The name offered is the backup's (`Backup 2026-11-16 10.00 - Documents - report.txt`).
- Where the program can't show the plugin's windows (before RustySalamander 0.9.4), the summary comes as a message, as before but shorter.
- **Check tapes logs every file it read** in `ltocatalog.log` (`verify LTO001L5: "path" 123456 bytes read`; one that isn't whole, too), and the average reading speed of each tape at its end. Before, only the files that couldn't be read were logged.
- **Check tapes shows the average speed** in the status bar, as a backup does: data read so far over the time spent reading it (loading the tape not counted).

## 0.4.1-alpha (2026-11-15)

- **A tape whose index can't be read in one partition is still read:** LTFS keeps the index (and the label) in both partitions; when one can't be read, the other is used, and a warning says so before the job goes on. The tape is used read-only. Before, the tape couldn't be used at all. The warning is a modal window from RustySalamander 0.9.10. If the copy used is older than what the catalog lists, the catalog isn't set back, and the warning says which files can't be read from the tape.
- **Check tapes** (*Plugins → Check tapes…*, also on tapes' context menu; `ltocat verify <barcode>...`): reads every file of the tapes ticked and lists those that can't be read, going on past them. Nothing is written.
- **What the drives report (TapeAlert)** is read after each use of a drive and shown with the command's result: a damaged or worn-out tape, a drive that needs cleaning, hardware faults.
- **Damaged tapes are noted in the catalog:** read and write failures, an index that can't be read, files a check couldn't read, and what TapeAlert says about a tape. The tape lists show "Damaged: …"; a check that reads the whole tape without a fault removes it.
- **A backup goes on after a tape fails:** you are asked whether to go on with another tape; what the failed tape's last index doesn't hold is written onto the next one. Before, the whole backup stopped.
- **The backup report comes at the end whatever happened,** also when the backup stopped on an error (in the program's modal message window), listing the tapes that failed.
- **Simulator:** `ltocat sim-damage <barcode> a|b [<record>]` damages a simulated tape.

- **`ltocat.exe` comes with the plugin's updates:** it now lives in the plugin's folder, `plugins\ltocatalog`, so RustySalamander's update check (Help → Check for updates) brings it along with the plugin, starting with this release. There it finds the plugin's catalog and settings by itself, as it did next to `rustysalamander.exe`. Delete the old `ltocat.exe` next to `rustysalamander.exe`: it is no longer updated. To type just `ltocat`, add `plugins\ltocatalog` to PATH.
- **Diacritics in the logs show in every editor:** `ltocatalog.log` and `ltocat.log` now start with the UTF-8 byte order mark. Without it, editors that guess read the file in the Windows code page, and names came out garbled (`PĹ™ĂlĂš` for `Příliš`). The text itself was always right; a log started by an earlier version gets the mark when it is next begun anew (at 64 MB, or after you delete it).
- **The detailed log reads plainly:** values are written as values, not as program internals. Sizes are bytes with a readable size (`130024472576 bytes (121.1 GiB) of 1443999252480 bytes (1.3 TiB)`, not `Some(130024472576) of Some(1443999252480) bytes`); something not known says `unknown` or `none`; times are dates; paths have single backslashes; lists of tapes are barcodes separated by commas; errors are the message.
- **The average speed in the status bar:** a backup's step now ends with how fast data has been going onto the tapes so far, on average (loading tapes and writing indexes not counted), and moves on every second inside a large file too.
- **Folders' Archive attribute:** a backup now clears it on the folders it puts on the tape too, not only on files (unless the folder changed meanwhile). *Only files with the Archive attribute* also takes the folders whose attribute is set, an empty one too; the other folders are made on the tape only where a changed file goes.
- **Detailed log:** `ltocatalog.log` in the plugin's data folder (next to `catalog.sqlite`), on by default; *Library settings…* can turn it off. It holds every SCSI command with its bytes, status, sense and time (records of data that went well are summed up: how many, how many bytes, how fast), what each tape's two partitions hold when it is mounted (which index, which generation, where), every index written, what the catalog holds for a tape compared with the tape, which drive a tape was loaded into and what its cartridge memory says, each file a backup writes and where, the plugin's commands, questions and answers, and every error. The file is cut at 64 MB; four older ones are kept (`ltocatalog.1.log`…). `ltocat` writes `ltocat.log` there, or where `--log <file>` says.
- **Shorter stops during a backup:** the index a backup writes every 100 GB now goes only to the end of the data, as IBM LTFS does while a tape is mounted; it takes a few seconds. Before, the tape also moved to the index partition and back each time, a minute or two. At the end of each tape (and on Cancel) the index is written into both partitions, as before. After a failure in between, the newest index is at the end of the data, where this plugin and IBM LTFS find it.
- **Library settings → "During a backup, every 100 GB, write the index":** *at the end of the data only* (the default) or *into the index partition too* (as 0.4.0 did). `ltocat backup … --index data|both` overrides it.
- **The wrong cartridge is never recorded or written:** when the tape mounted as a barcode holds another LTFS volume than the catalog has for that barcode, the job stops with nothing written, and the catalog is left alone. Before, this counted as "the catalog was out of date": the catalog row was overwritten with the other tape's index. The message names the tape the volume belongs to.
- **A backup goes on when only the catalog is behind:** the same volume, with a newer index on the tape than in the catalog, is taken from the tape and the backup continues. Deleting and renaming still stop there, so you can look first. A new tape the catalog says is empty is read before it is used.

## 0.4.0-alpha (2026-11-14)

- **Back up a folder:** *Plugins → Back up a folder…* copies a folder onto tapes, everything or only the files with the Archive attribute set, into `Backup <date> <time>\<folder name>` on each tape used. The attribute is cleared after a file is on tape (a tick box, on by default: incremental; off: differential).
  - The form lists the LTFS tapes in the libraries and drives with their free space and notes; the ticked ones are filled in barcode order.
  - A file goes onto a tape whole: one that doesn't fit on the rest of a tape goes onto the next. When the tapes are full, a new one is asked for in the mail slot: an empty LTFS tape is used, a tape with files refused, and a blank one formatted only after you confirm.
  - A file larger than a whole tape, or one that can't be read, is left out and listed in the report. Cancel keeps what was written, with its index; the index is also written every 100 GB.
  - `ltocat backup <folder> [<barcode>...] [--archive-only] [--keep-archive]` does the same from the command line.
- **Notes in the tape lists:** *Format tapes…* (and the backup form) show each tape's note and free space.

## 0.3.9-alpha (2026-11-14)

- The command *Put the tapes in the drives back* is now called *Unload tapes from drives*.
- **Retry looks for the tape for up to 30 seconds:** a tape put into the mail slot shows up only once the library has read it. After Retry, the libraries and drives are now looked at every few seconds, and the tape is used as soon as it is there; the question comes again only if it hasn't shown up in 30 seconds (longer while a library says it is still reading its mail slot, at most 3 minutes). Cancel stops the wait.

## 0.3.8-alpha (2026-11-14)

- **"READ ELEMENT STATUS: unit attention (28/01)" no longer stops a command:** a library answers its next command so after a tape was taken out of or put into its mail slot, to say its contents changed; the command wasn't carried out. The robot's commands (inventory, moves) are now sent again after it (and after "becoming ready"), up to five times. Before, Eject right after taking a tape out of the mail slot failed with this message.
- **Eject waits for the mail slot:** while the robot can't reach it (its door open, or the library still taking in what was done there), Eject waits for it, up to 3 minutes, and Cancel stops the wait. A mail slot that is full says so: take the tape out first.

## 0.3.7-alpha (2026-11-14)

- **Retry pressed before the library has read its mail slot:** after its door is closed, a library needs a while before it reports the tape put there (an IBM TS3100 about a minute). Retry pressed meanwhile now waits for it, up to 3 minutes, with "waiting for the library to read its mail slot" as the step, and then loads the tape; the same before asking for a tape that was just put in. Before, the job went on with what the library said halfway through.
- **Cancel of a copy works while a tape is being fetched:** a copy from or to a tape asks the copy's progress now and then while no data flows yet (the tape being found, loaded, or the library reading its mail slot), so Cancel in its window stops it. Before, Cancel took effect only once data flowed.
- **The Insert tape question is a dialog like the program's:** as tall as its text and buttons (no empty space under them), the margins and gaps of RustySalamander's dialogs, not resizable, and without minimize and maximize buttons.

## 0.3.6-alpha (2026-11-14)

- **Updates through RustySalamander:** Help → Check for updates (and the check at start) now offers newer LTO Catalog releases and installs the plugin folder. Earlier versions didn't say where their releases are, so they were never offered; this one has to be installed by hand once. `ltocat.exe` isn't part of a plugin update: take it from the release's full zip.

## 0.3.5-alpha (2026-11-14)

- **A drive Windows sees twice is one drive:** on an IBM TS3100, one LTO-5 SAS drive appeared as both `\\.\Tape0` and `\\.\Tape1` (the same serial number), while the library's other drive (Fibre Channel) wasn't connected to the computer. The plugin took them for the library's two drives, so a job for the tape in library drive 1 (a format, a copy) was done to the tape in drive 2. A device with the serial number of one already found is now left out, and a library drive that the changer identifies but no device matches is no longer matched by order: it isn't used, and Library status and `ltocat selftest` say so. `ltocat selftest` lists the library's drives by element, with what the changer says each one is and the tape device it is used through, or that it isn't used.
- **A tape in a library is checked before anything is done to it:** the cartridge in the drive must say it is the tape asked for (the barcode in its cartridge memory, else the volume serial of its LTFS label). If it says it is another one, nothing is read, written or formatted, the inventory is read again, and the message says what was found. Before, a drive that held another tape than the program thought (drives matched to the wrong tape devices, or tapes moved at the library's panel) got the job, so a format or a copy could go to the wrong tape.

## 0.3.4-alpha (2026-11-14)

- **Search the catalog:** Plugins → *Search the catalog…* finds files and folders by a part of their name or by a mask (`*.mkv`), on every tape or one, without loading a tape. The results open in RustySalamander's results window; files found are copied out of it (or a panel showing them) like out of the tapes, and the library loads what it needs.
- **Format several tapes in one job:** *Format tapes…* lists the tapes in the libraries and drives, with where each is and what it holds, to tick (those selected in the panel are ticked); another one can be given by its barcode. With no volume name given, each tape is named by its barcode. Typing ERASE confirms; the job formats them one after the other, with Cancel between tapes.
- **Right-click on tapes:** Search the catalog, Import, Format, Eject, Note about and Forget the tape (needs RustySalamander 0.9.9-beta or later).
- **The package's README and CHANGELOG are named `README-ltocatalog.md` and `CHANGELOG-ltocatalog.md`,** so unpacking it into the program's folder no longer overwrites RustySalamander's own.

## 0.3.3-alpha (2026-11-13)

- **Formatting on an IBM LTO-5 drive:** the drive refused the partition page (MODE SELECT: illegal request, 26/00). The page is now set as IBM's LTFS sets it: the drive's own answer sent back with the block descriptor, only the partition fields changed; the earlier way is tried next. If the drive refuses both, the message shows the drive's page, what was sent and which byte the drive refused, to pass on.
- A refused command now says which byte (and bit) the drive objected to.
- **Writing at the drive's speed:** every mode setting sent to the drive asked for unbuffered writing by mistake, so the drive confirmed each record only once it was on the tape and stopped the tape after every one: an LTO-5 wrote 0.2 MB/s instead of about 140. Writing is buffered now.

## 0.3.2-alpha (2026-11-13)

- **What it is doing, and Cancel:** a command (Inventory, Import tapes, Format, Eject, Status, Unload, Library settings) tells RustySalamander's status bar each step: looking for the devices, getting a tape into a drive, reading its LTFS volume, formatting, writing the index, which tape of how many. **Cancel** stops it at the next point it can: between tapes, or between files (never in the middle of a format); the tapes done stay imported. Other plugins work meanwhile. Needs RustySalamander 0.9.8-beta or later; older versions show "working…".
- `ltocat` prints the same steps as it goes.

## 0.3.1-alpha (2026-11-13)

- **A disk is never used as a tape drive or a changer:** a device named in *Library settings…* (or found) gets tape commands only if it says it is a tape drive, and changer commands only if it says it is a medium changer. Before, `\\.\PhysicalDrive0` typed as a drive would have received FORMAT on *Format a tape*. A device that isn't one is left out, with a note in *Library status*.
- **The catalog is never a device:** *Library settings…* and `ltocat --catalog` take only a whole path to a file in a folder that exists; `NUL`, `\\.\…`, `CON.sqlite` and folders are refused with the reason.
- **Typed input checked:** a barcode is letters A–Z and digits (at most 16); a volume name has no control characters (they would make the tape's index invalid XML for other LTFS software); a note is one line; device names have no control characters. Every form was tried with RustySalamander's list of hostile input.
- Built against RustySalamander 0.9.7-beta's plugin interface: a name that looks like a placeholder (`{error}`) shows as it is in messages.

## 0.3.0-alpha (2026-11-11)

- **Standalone drives:** drives outside any library are used for tapes put in by hand. The tape in one is recognised by the barcode in its cartridge memory, else by its LTFS volume (its UUID in the catalog, or the volume serial of its VOL1 label), once per cartridge. A missing tape is asked for with where it can go: a library's mail slot or a standalone drive, and what each drive holds. When every standalone drive holds another tape, the least recently used one is ejected first. The eject button is locked while a job uses the drive. A new tape in a standalone drive can be formatted; it gets its barcode in the cartridge memory.
- **Several libraries:** every changer found is used, each with the drives that report to it; the catalog says which library a tape is in, and the same barcode in two libraries is noted. *Library settings…* and `ltocat --changer` take several changers, comma-separated.
- **Libraries and drives connected later** are found by *Inventory the libraries and drives* and *Library status*.
- **Eject a tape…** (was *Move a tape to the mail slot…*): a standalone drive ejects the tape.
- **`ltocat`:** `selftest`, `inventory`, `load`, `eject`, `insert` work on every library and standalone drive; the simulator has `--sim-libraries`, `--sim-drives` and `sim-move` to move tapes by hand. A question whose input has ended (a redirected stdin) cancels instead of retrying forever.

## 0.2.0-alpha (2026-11-11)

- **Portable like RustySalamander:** the catalog and the settings are kept in the plugin's folder (`plugins\ltocatalog`), or in the user's `%APPDATA%\RustySalamander\plugins\ltocatalog` once RustySalamander is in multi-user mode. Before, the catalog was always in `%APPDATA%\RustySalamander\ltocatalog`, and the settings went there when the plugin's folder was read-only. A catalog from 0.1.0-alpha isn't picked up: move `catalog.sqlite` into the plugin's folder, or choose it in *Library settings…*.
- **`ltocat --program <folder>`** uses the catalog and library settings of the plugin in that RustySalamander folder, wherever the program keeps them. Next to `rustysalamander.exe`, `ltocat` does this by itself. Without either (or `--catalog`), commands that need the catalog say so instead of using `%APPDATA%`.

## 0.1.0-alpha (2026-11-11)

- First version: LTFS tapes in an LTO library browsed from an offline catalog; the robot loads tapes on demand; missing tapes are asked for in a window drawn by the program; copying from and to tapes, folders, renames, deletes; importing, formatting, the mail slot, notes about where tapes are kept.
- `ltocat`, the command line: checks the hardware, inventory, import, get, put, format.
- Tested on a simulated library only; see "Not tested yet on real hardware" in the README.
