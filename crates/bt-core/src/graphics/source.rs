//! The pixels of a transmission that are not in the escape sequence: a file
//! the program names (`t=f`), a temporary file it leaves to be taken (`t=t`)
//! or a POSIX shared memory object (`t=s`).
//!
//! **The name comes from whatever is writing to the terminal**, which is not
//! always a program the user started: anything that can print can name a file
//! (`cat` of a downloaded page is enough). The rules are kitty's, and each
//! one closes a way for that text to do something the user did not ask for:
//!
//! - The policy is on the **canonical path** and it is applied **before** the
//!   file is opened — opening can have effects (a device, a pipe's other end)
//!   and whether it works tells the program the file exists. `/proc`, `/sys`
//!   and `/dev` (but `/dev/shm`, a temporary directory on Linux) are never
//!   read; a relative name is refused, because nothing here says what it is
//!   relative to; and what the canonical path names must be a regular file
//!   before it is opened.
//! - It is then opened without following a link at its last step, and what
//!   was opened must be what the path names (a swap between the check and the
//!   open is caught by comparing the two).
//! - A temporary file (`t=t`) is read only inside a temporary directory and
//!   only if its name carries `tty-graphics-protocol`, and is deleted once it
//!   is read, whether or not the read worked; a file that is refused, or whose
//!   command was refused before the file was looked at, is left alone. A
//!   shared memory object is removed once it is opened.
//! - **No answer carries anything about the file.** Every failure after a
//!   name was acted on is the same [`Error::unreadable`], with no name and no
//!   size in it: an answer is written into the PTY's input, so a name echoed
//!   back would be typed into whatever reads it, and a size or a reason would
//!   be the answer to "does this machine have this file".
//!
//! **Known limits**, each one kitty's too: whether a read works still tells
//! the program the file is there and, with `O=` and `S=`, how long it is, to
//! whatever is writing to the terminal (an `ssh` session included — the
//! program's own `t=f` test is what keeps it from naming files that are not
//! on this machine); any shared memory object of the user's can be removed
//! by naming it (the protocol makes the terminal its owner once it is
//! named); the check of the path and the open are two steps, and a directory
//! on the way can be swapped for a link between them (only the last step is
//! opened without following); and a file on a slow or stalled mount (a
//! network share, a placeholder that has to be fetched) can hold the open
//! for as long as the mount takes.
//!
//! Reading happens where decoding does ([`super::receive::decode_payload`]),
//! never in `TappedPty::read`; the one exception is [`peek`], the first
//! bytes of a file, which tell how far the cursor moves.

use std::ffi::{CString, OsStr};
use std::fs::{File, Metadata, OpenOptions};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileExt, MetadataExt, OpenOptionsExt};
use std::path::{Component, Path, PathBuf};
use std::sync::OnceLock;

use super::command::{Command, Medium};
use super::{Error, NAME_LIMIT};
use crate::shell::decode_base64;

/// What a temporary file's name must carry to be taken and deleted.
const TEMP_MARK: &[u8] = b"tty-graphics-protocol";

/// The name a command carries, from its base64 text.
pub(super) fn name(text: &[u8]) -> Result<Vec<u8>, Error> {
    let mut bytes = Vec::with_capacity(text.len() / 4 * 3 + 3);
    if decode_base64(text, &mut bytes).is_none() {
        return Err(Error::invalid("the payload is not valid base64"));
    }
    if bytes.is_empty() || bytes.len() > NAME_LIMIT || bytes.contains(&0) {
        return Err(Error::invalid("the name cannot be used"));
    }
    Ok(bytes)
}

/// The bytes the command names, at most `limit` of them: from `O=`, `S=`
/// long (or to the end). The file or object is gone afterwards if the medium
/// says so.
pub(super) fn load(command: &Command, name: &[u8], limit: usize) -> Result<Vec<u8>, Error> {
    let read = match command.medium {
        Medium::File | Medium::Temp => read_file(command, name, limit),
        Medium::Shared => read_shared(command, name, limit),
        Medium::Direct => None,
    };
    read.ok_or_else(Error::unreadable)
}

/// The first `len` bytes, from `O=`, of the file a command names; `None`
/// where the file would not be read, or for shared memory, which is not
/// looked into. Nothing is deleted.
pub(super) fn peek(command: &Command, name: &[u8], len: usize) -> Option<Vec<u8>> {
    if !matches!(command.medium, Medium::File | Medium::Temp) {
        return None;
    }
    let (_, file) = open(name, command.medium == Medium::Temp)?;
    read_range(&file, u64::from(command.data_offset), 0, len, false)
}

fn read_file(command: &Command, name: &[u8], limit: usize) -> Option<Vec<u8>> {
    let temp = command.medium == Medium::Temp;
    let (path, file) = open(name, temp)?;
    let bytes = read_range(
        &file,
        u64::from(command.data_offset),
        u64::from(command.data_size),
        limit,
        false,
    );
    // Deleted whether or not it could be read: the program handed it over.
    if temp {
        remove_if_same(&path, &file);
    }
    bytes
}

/// Whether two sets of file metadata are one file.
fn same_file(a: &Metadata, b: &Metadata) -> bool {
    a.dev() == b.dev() && a.ino() == b.ino()
}

/// Applies the path policy and opens the file it names.
fn open(name: &[u8], temp: bool) -> Option<(PathBuf, File)> {
    let path = Path::new(OsStr::from_bytes(name));
    if !path.is_absolute() {
        return None;
    }
    let path = std::fs::canonicalize(path).ok()?;
    if !readable(&path) {
        return None;
    }
    if temp && !(in_temp_dir(&path) && named_for_graphics(&path)) {
        return None;
    }
    // Only a regular file is opened: a pipe or a socket is not, however
    // it is opened.
    if !std::fs::symlink_metadata(&path).ok()?.is_file() {
        return None;
    }
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW | libc::O_NOCTTY)
        .open(&path)
        .ok()?;
    let opened = file.metadata().ok()?;
    let named = std::fs::symlink_metadata(&path).ok()?;
    (opened.is_file() && same_file(&opened, &named)).then_some((path, file))
}

/// Whether a canonical path is one that may be read at all: not `/proc`, not
/// `/sys`, and in `/dev` only `/dev/shm`.
fn readable(path: &Path) -> bool {
    let mut parts = path.components();
    if parts.next() != Some(Component::RootDir) {
        return false;
    }
    let Some(Component::Normal(first)) = parts.next() else {
        return false;
    };
    match first.as_bytes() {
        b"proc" | b"sys" => false,
        b"dev" => {
            matches!(parts.next(), Some(Component::Normal(second)) if second == "shm")
                && parts.next().is_some()
        }
        _ => true,
    }
}

/// The temporary directories, canonical, found once: the environment does not
/// change under us (nothing here sets it) and resolving them is four path
/// lookups, which `peek` would otherwise do in the read.
fn temp_dirs() -> &'static [PathBuf] {
    static DIRECTORIES: OnceLock<Vec<PathBuf>> = OnceLock::new();
    DIRECTORIES.get_or_init(|| {
        [
            PathBuf::from("/tmp"),
            PathBuf::from("/dev/shm"),
            std::env::temp_dir(),
        ]
        .iter()
        .filter_map(|directory| std::fs::canonicalize(directory).ok())
        .collect()
    })
}

/// Whether a canonical path is inside one of the temporary directories.
fn in_temp_dir(path: &Path) -> bool {
    in_any_of(path, temp_dirs())
}

/// Whether `path` is inside one of the (canonical) `directories`, by parts
/// and not by letters, and is not one of them.
fn in_any_of(path: &Path, directories: &[PathBuf]) -> bool {
    directories
        .iter()
        .any(|directory| path != directory && path.starts_with(directory))
}

/// Whether a file's own name (the last part) says it was made for this.
fn named_for_graphics(path: &Path) -> bool {
    path.file_name().is_some_and(|name| {
        name.as_bytes()
            .windows(TEMP_MARK.len())
            .any(|window| window == TEMP_MARK)
    })
}

/// Deletes `path` if it is still the file that was read.
fn remove_if_same(path: &Path, file: &File) {
    let (Ok(opened), Ok(named)) = (file.metadata(), std::fs::symlink_metadata(path)) else {
        return;
    };
    if same_file(&opened, &named) && named.is_file() {
        let _ = std::fs::remove_file(path);
    }
}

/// `size` bytes (0: to the end) from `offset`, not more than `limit` and not
/// more than the file has, so what the program says never sizes an
/// allocation beyond what is there to read. `object`: it is shared memory,
/// which macOS maps instead of reading.
fn read_range(file: &File, offset: u64, size: u64, limit: usize, object: bool) -> Option<Vec<u8>> {
    let length = file.metadata().ok()?.len();
    let want = wanted(length, offset, size, limit)?;
    copy(file, offset, want, object)
}

#[cfg(target_os = "macos")]
fn copy(file: &File, offset: u64, want: usize, object: bool) -> Option<Vec<u8>> {
    if object {
        read_mapped(file, offset, want)
    } else {
        read_at(file, offset, want)
    }
}

#[cfg(not(target_os = "macos"))]
fn copy(file: &File, offset: u64, want: usize, _object: bool) -> Option<Vec<u8>> {
    read_at(file, offset, want)
}

fn wanted(length: u64, offset: u64, size: u64, limit: usize) -> Option<usize> {
    let available = length.saturating_sub(offset);
    let wanted = if size > 0 {
        size.min(available)
    } else {
        available
    };
    Some(usize::try_from(wanted).ok()?.min(limit))
}

fn read_at(file: &File, offset: u64, want: usize) -> Option<Vec<u8>> {
    let mut bytes = vec![0; want];
    let mut done = 0;
    while done < want {
        match file.read_at(&mut bytes[done..], offset + done as u64) {
            Ok(0) => break,
            Ok(count) => done += count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => return None,
        }
    }
    bytes.truncate(done);
    Some(bytes)
}

/// A shared memory object's name: a `/` and then the name, with no further
/// `/` (POSIX leaves such names to the system, and glibc refuses them).
fn shared_name(name: &[u8]) -> Option<CString> {
    let rest = name.strip_prefix(b"/")?;
    if rest.is_empty() || rest == b"." || rest == b".." || rest.contains(&b'/') {
        return None;
    }
    CString::new(name).ok()
}

fn read_shared(command: &Command, name: &[u8], limit: usize) -> Option<Vec<u8>> {
    let name = shared_name(name)?;
    let object = open_shared(&name)?;
    let bytes = read_range(
        &object,
        u64::from(command.data_offset),
        u64::from(command.data_size),
        limit,
        true,
    );
    // The object goes whether or not it could be read: the program handed it
    // over, and one that is not removed stays in the system until a restart.
    // SAFETY: `name` is a NUL-terminated string.
    unsafe { libc::shm_unlink(name.as_ptr()) };
    bytes
}

/// macOS keeps shared memory objects in the kernel, under no path.
#[cfg(target_os = "macos")]
fn open_shared(name: &CString) -> Option<File> {
    use std::os::fd::FromRawFd;

    // SAFETY: `name` is a NUL-terminated string that outlives the call; the
    // flags ask for read access to an object that already exists.
    let fd = unsafe { libc::shm_open(name.as_ptr(), libc::O_RDONLY, 0) };
    if fd < 0 {
        return None;
    }
    // SAFETY: `fd` was just returned by `shm_open` and nothing else owns it.
    Some(unsafe { File::from_raw_fd(fd) })
}

/// Elsewhere an object is a file in `/dev/shm`, which anyone may put a pipe
/// in; `shm_open` would open that and wait for a writer, so the file is
/// opened the way any other is.
#[cfg(not(target_os = "macos"))]
fn open_shared(name: &CString) -> Option<File> {
    let bytes = name.as_bytes();
    let path = Path::new("/dev/shm").join(OsStr::from_bytes(bytes.strip_prefix(b"/")?));
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW | libc::O_NOCTTY)
        .open(&path)
        .ok()?;
    file.metadata().ok()?.is_file().then_some(file)
}

/// macOS cannot `read` a shared memory object, so it is mapped and copied. A
/// mapping of a file that is cut short while it is read raises `SIGBUS`; a
/// shared memory object cannot be resized there once it has its size, which
/// is why this is safe for objects and is not done for files.
#[cfg(target_os = "macos")]
fn read_mapped(object: &File, offset: u64, want: usize) -> Option<Vec<u8>> {
    use std::os::fd::AsRawFd;

    if want == 0 {
        return Some(Vec::new());
    }
    // SAFETY: `sysconf` reads a system constant.
    let page = u64::try_from(unsafe { libc::sysconf(libc::_SC_PAGESIZE) })
        .ok()
        .filter(|&page| page > 0)?;
    let start = offset - offset % page;
    let skip = usize::try_from(offset - start).ok()?;
    let length = want.checked_add(skip)?;
    let start = libc::off_t::try_from(start).ok()?;
    // SAFETY: a read-only shared mapping of `length` bytes at a page-aligned
    // offset of an open object; the result is checked before it is used.
    let map = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            length,
            libc::PROT_READ,
            libc::MAP_SHARED,
            object.as_raw_fd(),
            start,
        )
    };
    if map == libc::MAP_FAILED {
        return None;
    }
    // SAFETY: the mapping is `length` bytes long and `skip + want == length`
    // (`want` was clamped to the object's size by `wanted`); it stays mapped
    // until the `munmap` below.
    let bytes = unsafe { std::slice::from_raw_parts(map.cast::<u8>().add(skip), want) }.to_vec();
    // SAFETY: unmaps exactly what was mapped, once.
    unsafe { libc::munmap(map, length) };
    Some(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(text: &str) -> PathBuf {
        PathBuf::from(text)
    }

    #[test]
    fn the_places_that_are_never_read() {
        for denied in [
            "/proc/self/status",
            "/sys/class",
            "/dev/null",
            "/dev/zero",
            "/dev/shm",
            "/",
        ] {
            assert!(!readable(&path(denied)), "{denied}");
        }
        for allowed in [
            "/tmp/a.png",
            "/private/tmp/a.png",
            "/Users/me/a.png",
            "/dev/shm/a",
            "/proc2/a",
        ] {
            assert!(readable(&path(allowed)), "{allowed}");
        }
        assert!(!readable(&path("relative/a.png")));
    }

    #[test]
    fn a_name_is_inside_a_directory_by_its_parts_not_its_letters() {
        let directories = [path("/var/tmp"), path("/tmp")];
        assert!(in_any_of(&path("/tmp/a"), &directories));
        assert!(in_any_of(&path("/var/tmp/x/a"), &directories));
        assert!(
            !in_any_of(&path("/tmp"), &directories),
            "the directory itself"
        );
        assert!(!in_any_of(&path("/tmpx/a"), &directories), "same letters");
        assert!(!in_any_of(&path("/home/me/a"), &directories));
        assert!(!in_any_of(&path("/a"), &[]));
    }

    #[test]
    fn a_temporary_file_is_named_for_the_protocol_in_its_own_name() {
        assert!(named_for_graphics(&path("/tmp/tty-graphics-protocol-abc")));
        assert!(named_for_graphics(&path("/tmp/x-tty-graphics-protocol")));
        assert!(!named_for_graphics(&path(
            "/tmp/tty-graphics-protocol/other"
        )));
        assert!(!named_for_graphics(&path("/tmp/image.png")));
    }

    #[test]
    fn a_shared_memory_name_is_a_slash_and_a_word() {
        assert!(shared_name(b"/icat-1").is_some());
        for bad in [
            &b"icat"[..],
            b"/",
            b"",
            b"/a/b",
            b"/.",
            b"/..",
            b"//a",
            b"/a\0b",
        ] {
            assert!(shared_name(bad).is_none(), "{bad:?}");
        }
    }

    #[test]
    fn what_is_read_is_what_is_asked_and_what_is_there() {
        // length, offset, size, limit
        assert_eq!(wanted(100, 0, 0, 1000), Some(100));
        assert_eq!(wanted(100, 10, 0, 1000), Some(90));
        assert_eq!(wanted(100, 10, 20, 1000), Some(20));
        assert_eq!(
            wanted(100, 10, 500, 1000),
            Some(90),
            "past the end is the end"
        );
        assert_eq!(wanted(100, 200, 0, 1000), Some(0), "an offset past the end");
        assert_eq!(wanted(100, 0, 0, 40), Some(40), "the limit");
    }
}
