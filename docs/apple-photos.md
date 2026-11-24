# Add to Apple Photos (macOS)

An export can hand the files it wrote to Apple Photos, optionally into an album (issue #236, parity row
`LR-EXP-PHOTOS`). macOS only: on other platforms the option is hidden in the Export dialog and refused by
`app.export`. The web build doesn't include it.

> **Tested end to end** on macOS 26.6.2 with Photos 11.0, using the ad-hoc-signed packaged app and a new, empty
> Photos library: the [manual test](#manual-test) below, except the two `lightcraft-cli` steps (11 and 12). Please
> run it again after changing the script or the packaging.

## Using it

- **Export dialog:** tick **Add to Apple Photos** under *If file exists* and, if you like, type an **Album**
  name (empty = no album; see [Which album](#which-album)). The files are exported as usual, into the folder
  you chose, and then added to Photos. The toast says how many Photos took, or why it took none.
- **Export with Previous** and saved export presets repeat it (the setting is part of the export options).
- **Agents and scripts:** `app.export {…, addToPhotos: true, photosAlbum?: "Name"}` (desktop app, MCP `export`
  tool, `lightcraft-cli run`); `lightcraft-cli render in.dng -o out.jpg --opt addToPhotos=true --opt
  photosAlbum=Name`. The export's result carries `applePhotos` (below). The files are written either way: a
  failed Photos step doesn't fail `app.export` (its `applePhotos` has the `error`), while `lightcraft-cli render`
  says the file was written and exits with the error.
- **Files already on disk:** `export.addToPhotos {paths: [absolute paths], album?, wait?}`.
- **How an import went:** `export.photosImports` lists this session's imports, oldest first;
  `export.photosImports {job}` gives one.

### Waiting, or not

An import can take minutes, and the first one waits for the user to answer macOS's permission prompt. Each
import is a job: `{job, running, requested, album}`, plus, once Photos is done, `{imported, ids, warning?}`,
`{error}` or `{skipped}` (`exporting: true` while an export that will add its files is still writing them).
`skipped` says why Photos wasn't asked, in words; `cancelled: true` comes with it when the export was cancelled.

While Photos works, the activity stack ([background-tasks.md](background-tasks.md)) shows it. An export adding its
files keeps its own row, which reads "Adding to Apple Photos…" and loses its ✕, since Photos can't be stopped once
asked. An import of its own (`export.addToPhotos`, or the Photos step left running after `app.export` without
`background`) gets a row of kind `applePhotos`, "Adding to Apple Photos", with the album as its detail, no ✕ and no
count, as Photos reports none until it is done.

- In the **desktop app** nothing waits for Photos on the window's thread. The Export dialog's background export
  adds the files on its worker and reports them in its toast. `app.export` without `background` and
  `export.addToPhotos` answer at once with the running job (`running: true`); the app shows a toast when Photos
  is done, and `export.photosImports {job}` gives the outcome. (`export.addToPhotos {wait: true}` is refused there:
  the window's thread never waits for Photos.)
- **Without a window** (the MCP server, `lightcraft-cli`, the headless snapshot) everything waits for Photos by
  default: `app.export` and `export.addToPhotos` return the finished job, so a one-shot command never ends in
  the middle of an import. `export.addToPhotos {wait: false}` returns the running job instead (for a
  long-running MCP server that polls `export.photosImports`); a process that is about to end (the CLI after
  its last command, the MCP server at end of input, a snapshot script, also when they end with an error)
  first waits for such an import to finish. The wait follows the import: while an export is still writing the
  files it will add, up to an hour more of writing; once Photos is asked, the import's own time limit (see the
  table below) and half a minute; never more than three hours and one minute in all.
- **One import at a time.** An export that will add to Photos reserves the import before it writes anything
  and holds it until it hands its files over (or is cancelled, fails or writes nothing). While an import is
  reserved or running, another (`export.addToPhotos`, or an export asking for Photos, refused before it writes
  anything) is refused with the number of the one in the way, so a retry never sends the same files twice and
  an export is never turned away after writing its files. When LightCraft stops waiting (see the table below),
  Photos may still be importing: look in Photos before trying again.

### Which album

The album is looked for by its exact name (case included) among the albums at the **top level** of the
Photos library. Albums inside folders are never used: when the only album of that name is in a folder, a new
top-level album is made. When none is there, a new top-level album is made. When **more than one** top-level
album has the name, nothing is imported and LightCraft says so: rename one of them in Photos, or use another
name. (Photos' own list of albums includes those in folders, in no particular order, so the script checks
each one's parent folder.)

Photos either copies the files into its library or references them where they are, as set in Photos ▸
Settings ▸ General ▸ *Copy items to the Photos library*. When it references them, keep the exported files.

## How it works

LightCraft runs `/usr/bin/osascript` with a constant AppleScript (`SCRIPT` in
`crates/engine/src/apple_photos.rs`) that asks Photos, through its scripting interface, to import the files:
`import … into album … skip check duplicates false`. The album is found or made first ([Which
album](#which-album)). The script returns the new media items' ids. Files go to Photos 200 at a time (each path is an osascript argument, and
macOS limits a program's arguments to 1 MiB), one run after another within the import's time limit; the first
run makes the album when it is missing.

This is Apple Events to Photos' own import command. It is not the OS-level synthetic input that AGENTS.md
forbids: nothing is typed or clicked, and no other window is touched.

The album name and the file paths are never written into the script. They follow it as separate arguments
(`on run argv`), each passed to osascript unchanged, so quotes, backslashes, line breaks or a leading `-` in a
name or path can't change what the script does. A constant first argument keeps osascript from reading a
name that starts with `-` as one of its own options.

The first time, macOS asks whether LightCraft may control Photos. The answer is kept in System Settings ▸
Privacy & Security ▸ Automation. macOS asks about the app that started LightCraft's work: the LightCraft app
when it was opened from the Finder or with `open`, but the **terminal** for `lightcraft-cli` and for a
LightCraft started from a terminal (`cargo run`).

**Packaged app.** The release app is signed with the hardened runtime. For it to be allowed to ask, it carries
the `com.apple.security.automation.apple-events` entitlement (`packaging/macos/entitlements.plist`, also used
for `lightcraft-cli`), and its `Info.plist` has the `NSAppleEventsUsageDescription` text macOS shows in the
prompt (`packaging/macos/Info.plist.in`). Without them macOS refuses the events (`-1743`) without asking.
`cargo run` doesn't test this configuration; the manual test below uses the packaged app.

**Who can add what.** `export.addToPhotos {paths}` adds any file LightCraft can read, given its absolute path,
not only files it exported. So any client of the control port or the MCP server can put any readable image or
video on the Mac into the user's Photos library, and from there into iCloud Photos when that is on. macOS asks
for the Automation permission once per app, not per import, so after the first Allow nothing asks again. The
control port listens on 127.0.0.1 only and has no authentication ([control-protocol.md](control-protocol.md)),
so any program on this Mac that can connect to it can do this, including programs of other users logged in to
the Mac. The MCP server talks only to the program that started it (stdio). Treat giving an agent the MCP server
or the control port as giving it this too, and turn the control port on only when you need it.

## When it doesn't work

| What you see | Why, and what to do |
|---|---|
| "LightCraft isn't allowed to control Photos…" | Permission refused (`-1743`). Turn on Photos under LightCraft (or your terminal app) in System Settings ▸ Privacy & Security ▸ Automation, then try again. |
| "Apple Photos couldn't be found…" | Photos isn't installed or couldn't be opened (`-600`, `-10810`, `-10814`, or no `/System/Applications/Photos.app`). |
| "The import was cancelled in Photos." | Someone pressed Cancel in a Photos dialog (`-128`). |
| "More than one album at the top level of Photos is named …" | Several top-level albums have that name; nothing was added. Rename one in Photos, or use another name. |
| "Photos didn't finish adding the photos within N s…" / "didn't answer in time" | LightCraft waits ten minutes plus five seconds a file (at most two hours), then stops osascript. The ten minutes leave time to answer macOS's permission prompt and Photos' duplicates question. Photos may still be importing: look in Photos before trying again. |
| "Apple Photos is still adding N file(s) from an earlier request (import K)…" / "An export is about to add its files to Apple Photos (import K)…" | One import at a time: wait for import K (`export.photosImports {job: K}`), then try again. |
| "Photos added 2 of 3 files…" | A partial import: Photos skipped files it found to be duplicates (it may ask first) or couldn't read. |
| "Apple Photos is only available on macOS" | The setting came from a Mac (a preset or Export with Previous); the Export dialog drops it on other platforms. |

## Manual test

Run this once on a Mac whose Photos library you are happy to add a few test images to (or switch Photos to a
new empty library first: hold ⌥ while opening Photos ▸ Create New).

1. Package the app (ad-hoc signed unless `MACOS_SIGN_IDENTITY` is set): `packaging/macos/package.sh --arch
   aarch64` (or `x86_64` on an Intel Mac). Check its entitlement: `codesign -d --entitlements -
   target/macos-package/LightCraft.app` lists `com.apple.security.automation.apple-events`. With a Developer ID
   build, also run the notarized DMG from `dist/release/`.
2. Open it from the Finder, or with `open target/macos-package/LightCraft.app --args --demo` (not by running
   the binary in a terminal, which makes the terminal the app macOS asks about).
3. Select three photos, choose File ▸ Export…, set a folder (e.g. `~/Pictures/LightCraft Exports`), tick **Add
   to Apple Photos**, type the album `LightCraft test "1"` and press Export.
4. macOS asks whether **LightCraft** may control Photos, showing the usage text from `Info.plist`: allow it.
   Expected: the toast reads "Exported 3 of 3 photos · 3 added to Apple Photos album “LightCraft test "1"”",
   and a top-level album of that name holds the three images.
5. Export the same photos again with the same settings. Expected: Photos asks about duplicates or imports them
   again; the toast reports the count Photos returned (a partial import shows its warning).
6. Albums: in Photos make a folder holding an album `Nested`, then export into `Nested`. Expected: a new
   top-level album `Nested` gets the images, the one in the folder doesn't. Then make a second top-level album
   `Nested` and export again. Expected: nothing is added, and the toast says more than one album has the name.
7. Permission refused: in System Settings ▸ Privacy & Security ▸ Automation turn off Photos under LightCraft,
   export again. Expected: the files are written, and the toast says to allow LightCraft there. Turn it back
   on.
8. No album: keep Add to Apple Photos ticked, clear the Album field and export. Expected: the images land in the
   library, in no album.
9. Hostile names: album `-e`, then `a\b`, and an export folder whose name contains `"` and `'`; a name with a
   line break through the CLI: `lightcraft-cli run --demo app.export dir=/tmp/lc-photos addToPhotos=true
   "photosAlbum=$(printf 'two\nlines')"`. Expected: albums named exactly so; nothing else happens.
10. The window stays responsive: start the app with `open target/macos-package/LightCraft.app --args --demo
    --control 18999`, then send `{"method": "engine.execute", "params": {"command": "export.addToPhotos",
    "params": {"paths": ["/tmp/lc-photos/<file>.jpg"]}}}` to port 18999 (`docs/control-protocol.md`).
    Expected: an immediate `{"job": …, "running": true}`; the app keeps responding; a toast when Photos is done;
    `export.photosImports` then shows the outcome.
11. Agents: `lightcraft-cli run --demo app.export dir=/tmp/lc-photos addToPhotos=true photosAlbum=CLI` (allow
    the terminal under Automation when asked). Expected: `applePhotos.imported` in the printed result.
12. Files on disk: `lightcraft-cli run export.addToPhotos 'paths=["/tmp/lc-photos/<file>.jpg"]' album=CLI`.
    Expected: the command waits for Photos and prints `{"job": …, "running": false, "imported": 1, …}`.

Report the toast or JSON, and the macOS and Photos versions.

## Limitations

- Albums inside folders are never used ([Which album](#which-album)).
- Photos' duplicate check stays on, so Photos may ask about duplicates; what it skips shows as a partial import.
- Cancelling a running export cancels the Photos step too; a Photos import already started can't be stopped
  from LightCraft.
- One import runs (or is reserved by an export) at a time; a session remembers its last 32.
- Quitting the desktop app while Photos is importing doesn't wait for it: Photos may still finish the import,
  but LightCraft won't show the outcome.

## Where the commands are

**Photo ▸ Apple Photos** holds them all: Edit Apple Photos Item, Save Edit to Apple Photos (⌘⌥⇧P), Save All Edits
to Apple Photos, Restore Edit from Apple Photos and Sync with Apple Photos. For a photo from Photos, the bottom bar
also has two round buttons after Copy Edit Settings: **Sync** (two curved arrows) and **Save to Photos** (an arrow
into a tray), each enabled where it applies, its tooltip naming its shortcut.

Save Edit acts on the selection: the photo shown in the Edit tools, or every photo selected in the grid. **Save All
Edits** is library-wide: every photo from Photos whose LightCraft edit isn't saved there (edited, and its settings
not the ones last saved), items not copied yet copied first. Either way the photos go to Photos as **one change**,
so macOS asks once, not once a photo; a photo that can't be rendered (or that Photos edited since) fails alone and
the toast says how many were saved. The same submenu is in the right-click menu of a photo from Photos, in
the grid and in the filmstrip under the Edit tools, each item enabled where it applies to that photo (Edit for a
photo not copied yet, Save for an edited photo, Sync for a copy); other photos don't show it. Save never has to
wait to be offered: an edited photo whose copy isn't made yet is copied first (under its activity row), then saved.

## What comes over from Photos

A photo shown from Photos (the sidebar's Apple Photos section) carries what Photos says about it: its title,
caption, keywords and location, and a **Favourite** as 5 stars. Photos wins: each time the library is read
again, those fields of a photo not yet copied for editing are replaced with Photos' values, and a photo that is no
longer a favourite loses its 5 stars (ratings of 1–4 stars stay). Once a photo has been copied to edit it,
LightCraft leaves its metadata alone. Not carried over yet: people, place names and which albums it is in.

## Saving an edit back to Photos

> **Tested on a Mac** (macOS 27.3, packaged app, System Photo Library): the [manual test](#manual-test-macos-packaged-app)
> steps 1–3 (save), 7 (restore), 9 (an edit made in Photos is brought in) and 10 (both edits kept, and a revert in
> Photos brings back the original), and the Photo ▸ Apple Photos submenu and right-click menu. Also: photos edited in
> Photos show Photos' edit in LightCraft (grid, navigator and loupe) before they are copied, LightCraft follows edits
> and reverts made in Photos while it is open, and an edit made in LightCraft on such a photo goes back to Photos
> with Save Edit to Apple Photos alone (no Edit Apple Photos Item or Sync first). Every step of the manual test
> below passed, also 4 (save again after a revert), 5 (deny the change), 6 (an iCloud-only photo) and 8 (Restore
> skips an edit made in Photos). Please run them again after changing `crates/photokit` or these engine modules.
>
> In Photos, an edit LightCraft saved is undone with **Image ▸ Revert to Original**; Photos offers no Reset
> Adjustments for it, as for any other app's edit, since only LightCraft reads the settings stored with it.

A photo copied out of Photos to be edited (issue #1622: the first edit of an item from the sidebar's Apple
Photos section) can put its edit back on the asset it came from: **Photo ▸ Apple Photos ▸ Save Edit to Apple Photos**
(⌘⌥⇧P / Ctrl+Alt+Shift+P, `applePhotos.saveEdit`). Photos then shows the edited photo in place of the asset's current version, in every
album and on every device that syncs with iCloud Photos. Photos keeps the original: **Image ▸ Revert to
Original** in Photos brings it back. Unlike Add to Apple Photos, nothing new is added to the library.

How it works (`crates/engine/src/photos_save.rs`, `crates/photokit`):

1. The photo is rendered at full size as a JPEG (quality 95, Display P3, its metadata, upright) to a temporary
   file, on a worker under a row in the activity stack.
2. PhotoKit prepares the asset for editing (`requestContentEditingInput`; it downloads the asset from iCloud
   first when it isn't on this Mac, shown as progress in the row, and ✕ stops it).
3. The JPEG is copied to where PhotoKit wants it, with LightCraft's develop settings as the edit's adjustment
   data (format `ai.storyteller.lightcraft.develop`, version 1; settings over 1 MB are left out), and the change
   is committed (`performChangesAndWait`). macOS asks the user to allow LightCraft to modify the photo.
4. The temporary file is removed, whatever happened.

Only the System Photo Library can be changed, only in the packaged macOS app (PhotoKit needs its
`NSPhotoLibraryUsageDescription` and the photos-library entitlement), and only photos: a video, or a photo
Photos won't let apps edit (a shared album's), is refused. Photos that weren't copied from Photos, or that have
no edits, are skipped and said why. The asset is found by its UUID, so a library that is a copy of the System
Photo Library saves to the System Photo Library's asset.

Saving never replaces an edit made in Photos that LightCraft hasn't brought in yet ([Edits made in
Photos](#edits-made-in-photos)): it asks for Photo ▸ Apple Photos ▸ Sync with Apple Photos first. Not yet: no HDR (gain map)
rendition; Live Photos keep their motion unedited; libraries other than the System Photo Library.

## Edits made in Photos

Whichever app edited a photo last, that edit shows, and neither is lost. Photos' own edits can't become LightCraft
sliders (Photos keeps them in its own format), so when Photos shows an edit LightCraft didn't make, that **version of
the photo** becomes the photo's file in LightCraft (`Originals/From Apple Photos/<name> (Photos edit).jpeg`; the
earlier file stays), and LightCraft's settings build on it:

- Settings never saved to Photos stay on top: Photos' version doesn't have them.
- Settings saved to Photos are already in its version (Photos edits on top of the version LightCraft saved), so
  they start again from the default. If they changed after that save, they are first kept as the snapshot
  *LightCraft edit before the Photos edit*.
- Photos went back to the original (Revert to Original) while LightCraft builds on an earlier Photos edit: the
  original becomes the photo's file again, by the same rules.

When it happens: once a session when such a photo is open in the Edit tools (quietly; on a worker under an activity
row), and with **Photo ▸ Apple Photos ▸ Sync with Apple Photos** (`applePhotos.sync`), which says how it went. Photos is asked
only when the asset changed since LightCraft last looked (`PHAsset.modificationDate`), and then whose edit it shows
(LightCraft's adjustment data, as for Restore); a change that isn't a new edit (a favourite, a title, LightCraft's own
save) is noted and changes nothing. A version found while a slider is being dragged waits for the drag to end.
Bringing in Photos' version isn't an undo step (as copying a Photos item isn't). Restore puts settings back only onto
the version of the photo they build on (stored with them as `base`).

Building on Photos' version means editing its rendered JPEG or HEIC, not the raw original: there is less room for
large exposure or colour changes.

Photos not copied yet show Photos' edit too, read from the library's files (so for any library, not only the System
Photo Library): an asset Photos marks edited (`ZADJUSTMENTSSTATE` above 0 in `Photos.sqlite`; `ZHASADJUSTMENTS` in
older libraries) shows its full-size edited version, `resources/renders/<first character of the UUID>/<UUID>_1_201_a.heic`
(or `.jpeg`), in the grid too (its own small image; observed in a Photos 11 library on
macOS 27.3; Photos' previews in `resources/derivatives/` were seen to show the original of an edited photo, so they
aren't used for it). Editing it copies that version, and LightCraft's settings build
on it. When the edited version isn't on this Mac (Optimize Mac Storage), the original shows, as before. What LightCraft shows follows
Photos while it is open: while the Apple Photos view is shown, the library's database (`database/Photos.sqlite` and
its `-wal`) is looked at every 2 seconds, and when Photos changed it (an edit, a revert, a new photo) the view is
read again; a photo whose file changed drops what was decoded of the old one. A change made while another source is
shown is read when the view is shown again. Videos show
their original.

## Reading an edit back from Photos

**Photo ▸ Apple Photos ▸ Restore Edit from Apple Photos** (`applePhotos.restoreEdit`) gives a photo shown from Photos, or the copy
made to edit it, the develop settings LightCraft stored when it saved the edit Photos shows. Editing carries on
where it was: on another Mac that syncs with iCloud Photos, or after the LightCraft copy is gone. It is one undo
step, and nothing changes in Photos.

PhotoKit is asked for the asset's editing input with LightCraft's format
(`ai.storyteller.lightcraft.develop`) as the one it can handle (`canHandleAdjustmentData`), on a worker under a row
in the activity stack (Photos may download the photo from iCloud first; ✕ stops it). Photos hands the stored data
over only when the edit it shows is LightCraft's. The settings are checked like any from outside the app, and
values out of range are brought back into range. Skipped, and said why: an edit made in Photos or another app since
(or a photo never edited there), settings too large to have been stored, and photos not from Photos. Same limits as
saving: System Photo Library only, packaged macOS app only.

### Manual test (macOS, packaged app)

1. In the sidebar's Apple Photos section, open the System Photo Library and pick a photo. Raise its exposure in
   the Edit tools: an activity row copies it into the library.
2. Photo ▸ Apple Photos ▸ Save Edit to Apple Photos. Allow access to Photos if asked, then allow the change.
3. The toast says *Saved to Apple Photos*. In Photos, the photo shows the edit and has **Revert to Original**.
4. Revert in Photos, then save again from LightCraft: the edit comes back.
5. Deny the change when macOS asks: the toast says LightCraft was not allowed, and Photos is unchanged.
6. Pick a photo only in iCloud (Optimize Mac Storage), edit and save: the row shows the download, ✕ stops it.
7. Reset the photo's settings in LightCraft, then Photo ▸ Apple Photos ▸ Restore Edit from Apple Photos: the saved settings come
   back; Edit ▸ Undo takes them away again.
8. Edit the same photo in Photos (any adjustment), then Restore Edit again: it is skipped, saying the edit in
   Photos was not saved by LightCraft.
9. Open that photo in LightCraft's Edit tools (or Photo ▸ Apple Photos ▸ Sync with Apple Photos): it now shows Photos' edit, and
   the toast says the edit was brought in. Its LightCraft settings started again from the default (they were saved
   in step 2); with changes made after the save, they are in the snapshot *LightCraft edit before the Photos
   edit*.
10. Raise the exposure in LightCraft and save: Photos shows both edits. In Photos, Revert to Original, then sync in
    LightCraft: the original comes back.
