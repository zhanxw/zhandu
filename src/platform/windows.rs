use super::*;
use std::{
    collections::HashMap,
    ffi::{OsStr, c_void},
    fs::OpenOptions,
    os::windows::{ffi::OsStrExt, fs::OpenOptionsExt, io::AsRawHandle},
    ptr::{null, null_mut},
    sync::{Mutex, OnceLock},
};
use windows_sys::Win32::{
    Foundation::{ERROR_INSUFFICIENT_BUFFER, LocalFree},
    Security::{
        Authorization::{
            ConvertSidToStringSidW, ConvertStringSidToSidW, GetSecurityInfo, SE_FILE_OBJECT,
        },
        GROUP_SECURITY_INFORMATION, LookupAccountNameW, LookupAccountSidW,
        OWNER_SECURITY_INFORMATION, PSID,
    },
    Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT,
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES,
        FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_STANDARD_INFO, FileStandardInfo,
        GetFileInformationByHandle, GetFileInformationByHandleEx, READ_CONTROL,
    },
};

// IDs are process-local intern keys; SIDs are the persistent Windows identities.
#[derive(Default)]
struct Identities {
    ids: HashMap<String, u32>,
    sids: Vec<String>,
}
fn identities() -> &'static Mutex<Identities> {
    static IDENTITIES: OnceLock<Mutex<Identities>> = OnceLock::new();
    IDENTITIES.get_or_init(Mutex::default)
}
fn intern(sid: String) -> io::Result<u32> {
    let mut identities = identities()
        .lock()
        .map_err(|_| io::Error::other("identity registry poisoned"))?;
    if let Some(&id) = identities.ids.get(&sid) {
        return Ok(id);
    }
    let id = u32::try_from(identities.sids.len())
        .map_err(|_| io::Error::other("too many identities"))?;
    identities.sids.push(sid.clone());
    identities.ids.insert(sid, id);
    Ok(id)
}
pub fn owner_identifier(id: u32) -> Option<String> {
    identities().lock().ok()?.sids.get(id as usize).cloned()
}
struct LocalMemory(*mut c_void);
impl Drop for LocalMemory {
    fn drop(&mut self) {
        // SAFETY: this wrapper owns memory returned by Win32's LocalAlloc family.
        unsafe {
            LocalFree(self.0);
        }
    }
}
fn wide(value: &OsStr) -> Vec<u16> {
    value.encode_wide().chain(Some(0)).collect()
}

fn sid_string(sid: PSID) -> io::Result<String> {
    if sid.is_null() {
        return Err(io::Error::other(
            "file security descriptor has no owner/group SID",
        ));
    }
    let mut text = null_mut();
    // SAFETY: caller supplies a valid SID, and text receives an allocated UTF-16 string.
    if unsafe { ConvertSidToStringSidW(sid, &mut text) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let _memory = LocalMemory(text.cast());
    let mut len = 0;
    // SAFETY: ConvertSidToStringSidW returns a NUL-terminated string held alive above.
    unsafe {
        while *text.add(len) != 0 {
            len += 1;
        }
        Ok(String::from_utf16_lossy(std::slice::from_raw_parts(
            text, len,
        )))
    }
}

pub(crate) fn read_metadata(path: &Path) -> io::Result<EntryMetadata> {
    // Open the reparse point itself; never traverse junctions or symlink targets.
    let file = OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES | READ_CONTROL)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    let handle = file.as_raw_handle();
    // SAFETY: both structs are POD buffers populated by the corresponding Win32 calls.
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    let mut standard = FILE_STANDARD_INFO::default();
    // SAFETY: handle is owned by file and output buffers have the required layout/size.
    if unsafe { GetFileInformationByHandle(handle, &mut info) } == 0
        || unsafe {
            GetFileInformationByHandleEx(
                handle,
                FileStandardInfo,
                (&mut standard as *mut FILE_STANDARD_INFO).cast(),
                std::mem::size_of::<FILE_STANDARD_INFO>() as u32,
            )
        } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let (mut owner, mut group, mut descriptor) = (null_mut(), null_mut(), null_mut());
    // SAFETY: valid handle; all requested output pointers are writable. No ACLs requested.
    let error = unsafe {
        GetSecurityInfo(
            handle,
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | GROUP_SECURITY_INFORMATION,
            &mut owner,
            &mut group,
            null_mut(),
            null_mut(),
            &mut descriptor,
        )
    };
    if error != 0 {
        return Err(io::Error::from_raw_os_error(error as i32));
    }
    let _descriptor = LocalMemory(descriptor);
    let uid = intern(sid_string(owner)?)?;
    let gid = intern(sid_string(group)?)?;
    let reparse = info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0;
    Ok(EntryMetadata {
        dev: u64::from(info.dwVolumeSerialNumber),
        ino: (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
        nlink: u64::from(info.nNumberOfLinks),
        uid,
        gid,
        is_dir: info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0 && !reparse,
        is_symlink: reparse,
        usage: Usage {
            allocated: standard.AllocationSize.max(0) as u64,
            apparent: standard.EndOfFile.max(0) as u64,
            inodes: 1,
        },
    })
}

pub fn resolve_owner(value: &str, _: bool) -> Result<u32, String> {
    resolve(value)
        .map_err(|error| format!("cannot resolve Windows account or SID {value:?}: {error}"))
}
fn resolve(value: &str) -> io::Result<u32> {
    let value_wide = wide(OsStr::new(value));
    if value.starts_with("S-") {
        let mut sid = null_mut();
        // SAFETY: value_wide is a NUL-terminated string; sid is a writable output.
        if unsafe { ConvertStringSidToSidW(value_wide.as_ptr(), &mut sid) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let _sid = LocalMemory(sid);
        return intern(sid_string(sid)?);
    }
    let (mut sid_len, mut domain_len, mut kind) = (0, 0, 0);
    // SAFETY: zero-sized query with writable length outputs.
    unsafe {
        LookupAccountNameW(
            null(),
            value_wide.as_ptr(),
            null_mut(),
            &mut sid_len,
            null_mut(),
            &mut domain_len,
            &mut kind,
        );
    }
    if sid_len == 0 {
        return Err(io::Error::last_os_error());
    }
    // usize storage provides pointer alignment for the SID structure.
    let mut sid = vec![0usize; (sid_len as usize).div_ceil(std::mem::size_of::<usize>())];
    let mut domain = vec![0u16; domain_len as usize];
    // SAFETY: allocated buffers match the sizes returned by the first call.
    if unsafe {
        LookupAccountNameW(
            null(),
            value_wide.as_ptr(),
            sid.as_mut_ptr().cast(),
            &mut sid_len,
            domain.as_mut_ptr(),
            &mut domain_len,
            &mut kind,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    intern(sid_string(sid.as_mut_ptr().cast())?)
}

pub fn owner_name(id: u32, _: bool) -> Option<String> {
    let sid_text = owner_identifier(id)?;
    let sid_text_wide = wide(OsStr::new(&sid_text));
    let mut sid = null_mut();
    // SAFETY: valid NUL-terminated SID text and writable output.
    if unsafe { ConvertStringSidToSidW(sid_text_wide.as_ptr(), &mut sid) } == 0 {
        return Some(sid_text);
    }
    let _sid = LocalMemory(sid);
    let (mut name_len, mut domain_len, mut kind) = (0, 0, 0);
    // SAFETY: valid SID and zero-sized query buffers with writable length outputs.
    unsafe {
        LookupAccountSidW(
            null(),
            sid,
            null_mut(),
            &mut name_len,
            null_mut(),
            &mut domain_len,
            &mut kind,
        );
    }
    if io::Error::last_os_error().raw_os_error() != Some(ERROR_INSUFFICIENT_BUFFER as i32) {
        return Some(sid_text);
    }
    let (mut name, mut domain) = (
        vec![0u16; name_len as usize],
        vec![0u16; domain_len as usize],
    );
    // SAFETY: buffers are allocated to the lengths requested by the first call.
    if unsafe {
        LookupAccountSidW(
            null(),
            sid,
            name.as_mut_ptr(),
            &mut name_len,
            domain.as_mut_ptr(),
            &mut domain_len,
            &mut kind,
        )
    } == 0
    {
        return Some(sid_text);
    }
    let name = String::from_utf16_lossy(&name[..name_len as usize]);
    let domain = String::from_utf16_lossy(&domain[..domain_len as usize]);
    let name = if domain.is_empty() {
        name
    } else {
        format!("{domain}\\{name}")
    };
    Some(display_name(OsStr::new(&name)))
}
