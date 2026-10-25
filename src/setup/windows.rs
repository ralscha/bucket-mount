//! Per-user Windows registration. No administrator rights or script host is needed.

use anyhow::{Context, Result, bail, ensure};
use std::{
    ffi::{OsStr, OsString, c_void},
    fs, io,
    os::windows::ffi::{OsStrExt, OsStringExt},
    path::{Path, PathBuf},
    ptr::{null, null_mut},
};
use windows_sys::{
    Win32::{
        Foundation::{
            ERROR_FILE_NOT_FOUND, ERROR_MORE_DATA, ERROR_PATH_NOT_FOUND, RPC_E_CHANGED_MODE,
        },
        Storage::FileSystem::WIN32_FIND_DATAW,
        System::{
            Com::{
                CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
                CoTaskMemFree, CoUninitialize, STGM_READ,
            },
            Registry::{
                HKEY, HKEY_CURRENT_USER, KEY_READ, KEY_WRITE, REG_DWORD, REG_OPTION_NON_VOLATILE,
                REG_SZ, RegCloseKey, RegCreateKeyExW, RegDeleteKeyW, RegDeleteValueW,
                RegOpenKeyExW, RegQueryValueExW, RegSetValueExW,
            },
        },
        UI::Shell::{FOLDERID_Programs, SHGetKnownFolderPath, SLGP_RAWPATH, ShellLink},
    },
    core::{GUID, HRESULT, IUnknown_Vtbl},
};

const STARTUP_VALUE: &str = "BucketMount";

pub struct Registration {
    pub uninstall_key: String,
    pub run_key: String,
    pub shortcut: PathBuf,
}

impl Registration {
    pub fn for_current_user() -> Result<Self> {
        let mut folder = null_mut();
        unsafe {
            check_hr(SHGetKnownFolderPath(
                &FOLDERID_Programs,
                0,
                null_mut(),
                &mut folder,
            ))
            .context("find your Start menu Programs folder")?;
            let mut len = 0;
            while *folder.add(len) != 0 {
                len += 1;
            }
            let path = PathBuf::from(OsString::from_wide(std::slice::from_raw_parts(folder, len)));
            CoTaskMemFree(folder.cast());
            Ok(Self {
                uninstall_key: r"Software\Microsoft\Windows\CurrentVersion\Uninstall\BucketMount"
                    .into(),
                run_key: r"Software\Microsoft\Windows\CurrentVersion\Run".into(),
                shortcut: path.join("Bucket Mount.lnk"),
            })
        }
    }

    pub fn ensure_available(&self) -> Result<()> {
        ensure!(
            !self.shortcut.try_exists()? && RegKey::open(&self.uninstall_key)?.is_none(),
            "Bucket Mount is already registered. Uninstall it before running setup again"
        );
        if let Some(key) = RegKey::open(&self.run_key)? {
            ensure!(
                key.string(STARTUP_VALUE)?.is_none(),
                "A Bucket Mount startup entry already exists. Remove it before running setup"
            );
        }
        Ok(())
    }

    pub fn install(&self, bin: &Path, config: &Path, startup: bool) -> Result<()> {
        ensure!(
            bin.is_absolute() && config.is_absolute(),
            "installation paths must be absolute"
        );
        self.ensure_available()?;
        let app = bin.join("bucket-mount.exe");
        let uninstaller = bin.join("uninstall.exe");
        let mut arguments = OsString::from("--config ");
        arguments.push(quote(config.as_os_str()));
        arguments.push(" run");

        if let Some(parent) = self.shortcut.parent() {
            fs::create_dir_all(parent).context("create Start menu folder")?;
        }
        write_shortcut(&self.shortcut, &app, bin, &arguments)?;

        let key = RegKey::create(&self.uninstall_key)?;
        key.set_string("InstallLocation", bin.as_os_str())?;
        key.set_string("DisplayName", OsStr::new("Bucket Mount"))?;
        key.set_string("DisplayVersion", OsStr::new(env!("CARGO_PKG_VERSION")))?;
        key.set_string("Publisher", OsStr::new("Bucket Mount"))?;
        let mut icon = quote(app.as_os_str());
        icon.push(",0");
        key.set_string("DisplayIcon", &icon)?;
        let mut uninstall = quote(uninstaller.as_os_str());
        uninstall.push(" --uninstall");
        key.set_string("UninstallString", &uninstall)?;
        uninstall.push(" --quiet");
        key.set_string("QuietUninstallString", &uninstall)?;
        key.set_dword("NoModify", 1)?;
        key.set_dword("NoRepair", 1)?;

        let mut command = quote(app.as_os_str());
        command.push(" ");
        command.push(arguments);
        if startup {
            let key = RegKey::create(&self.run_key)?;
            key.set_string(STARTUP_VALUE, &command)?;
        }
        Ok(())
    }

    pub fn uninstall(&self, bin: &Path) -> Result<()> {
        let app = bin.join("bucket-mount.exe");
        self.remove_startup(&app)?;
        if self.shortcut.try_exists()? && shortcut_matches(&self.shortcut, &app)? {
            fs::remove_file(&self.shortcut).context("remove Start menu shortcut")?;
        }
        if let Some(key) = RegKey::open(&self.uninstall_key)? {
            let owned = key
                .string("InstallLocation")?
                .is_some_and(|location| paths_match(&location, bin.as_os_str()));
            drop(key);
            if owned {
                // RegDeleteKeyW deliberately refuses keys with subkeys.
                delete_key(&self.uninstall_key)?;
            }
        }
        Ok(())
    }

    fn remove_startup(&self, app: &Path) -> Result<()> {
        if let Some(key) = RegKey::open(&self.run_key)?
            && key
                .string(STARTUP_VALUE)?
                .is_some_and(|command| command_matches(&command, app))
        {
            key.remove_value(STARTUP_VALUE)?;
        }
        Ok(())
    }
}

fn wide(value: impl AsRef<OsStr>) -> Result<Vec<u16>> {
    let mut text: Vec<_> = value.as_ref().encode_wide().collect();
    ensure!(
        !text.contains(&0),
        "Windows paths and values cannot contain NUL"
    );
    text.push(0);
    Ok(text)
}

// Windows command-line quoting, including trailing backslashes before the closing quote.
fn quote(value: &OsStr) -> OsString {
    let mut result = vec![b'"' as u16];
    let mut slashes = 0;
    for unit in value.encode_wide() {
        if unit == b'\\' as u16 {
            slashes += 1;
        } else {
            result.extend(std::iter::repeat_n(
                b'\\' as u16,
                slashes * if unit == b'"' as u16 { 2 } else { 1 },
            ));
            if unit == b'"' as u16 {
                result.push(b'\\' as u16);
            }
            result.push(unit);
            slashes = 0;
        }
    }
    result.extend(std::iter::repeat_n(b'\\' as u16, slashes * 2));
    result.push(b'"' as u16);
    OsString::from_wide(&result)
}

fn paths_match(left: &OsStr, right: &OsStr) -> bool {
    fn units(path: &OsStr) -> Vec<u16> {
        path.encode_wide()
            .map(|unit| match unit {
                65..=90 => unit + 32,
                47 => 92,
                _ => unit,
            })
            .collect()
    }
    if units(left) == units(right) {
        return true;
    }
    // The shell can record a path in a different textual form (8.3 short names,
    // a link, or the long form of a parent). Resolve what still exists: the file
    // itself may already be deleted while its directory remains.
    fn resolved(path: &OsStr) -> Option<Vec<u16>> {
        let path = Path::new(path);
        if let Ok(resolved) = fs::canonicalize(path) {
            return Some(units(resolved.as_os_str()));
        }
        let mut resolved = units(fs::canonicalize(path.parent()?).ok()?.as_os_str());
        resolved.push(b'\\' as u16);
        resolved.extend(units(path.file_name()?));
        Some(resolved)
    }
    match (resolved(left), resolved(right)) {
        (Some(left), Some(right)) => left == right,
        _ => false,
    }
}

fn command_matches(command: &OsStr, app: &Path) -> bool {
    let command: Vec<_> = command.encode_wide().collect();
    // We only remove commands in the quoted format written by this installer.
    if command.first() != Some(&(b'"' as u16)) {
        return false;
    }
    let Some(end) = command[1..]
        .iter()
        .position(|&unit| unit == b'"' as u16)
        .map(|i| i + 1)
    else {
        return false;
    };
    if command
        .get(end + 1)
        .is_some_and(|&unit| unit != b' ' as u16 && unit != b'\t' as u16)
    {
        return false;
    }
    paths_match(&OsString::from_wide(&command[1..end]), app.as_os_str())
}

fn check_win32(code: u32) -> Result<()> {
    if code != 0 {
        return Err(io::Error::from_raw_os_error(code as i32).into());
    }
    Ok(())
}

fn check_hr(code: HRESULT) -> Result<()> {
    ensure!(code >= 0, "Windows returned HRESULT 0x{:08X}", code as u32);
    Ok(())
}

struct RegKey(HKEY);

impl RegKey {
    fn create(path: &str) -> Result<Self> {
        let path = wide(path)?;
        let mut key = null_mut();
        unsafe {
            check_win32(RegCreateKeyExW(
                HKEY_CURRENT_USER,
                path.as_ptr(),
                0,
                null(),
                REG_OPTION_NON_VOLATILE,
                KEY_READ | KEY_WRITE,
                null(),
                &mut key,
                null_mut(),
            ))?;
        }
        Ok(Self(key))
    }

    fn open(path: &str) -> Result<Option<Self>> {
        let path = wide(path)?;
        let mut key = null_mut();
        let status = unsafe {
            RegOpenKeyExW(
                HKEY_CURRENT_USER,
                path.as_ptr(),
                0,
                KEY_READ | KEY_WRITE,
                &mut key,
            )
        };
        if matches!(status, ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND) {
            return Ok(None);
        }
        check_win32(status)?;
        Ok(Some(Self(key)))
    }

    fn set_string(&self, name: &str, value: &OsStr) -> Result<()> {
        let name = wide(name)?;
        let value = wide(value)?;
        unsafe {
            check_win32(RegSetValueExW(
                self.0,
                name.as_ptr(),
                0,
                REG_SZ,
                value.as_ptr().cast(),
                (value.len() * 2).try_into()?,
            ))
        }
    }

    fn set_dword(&self, name: &str, value: u32) -> Result<()> {
        let name = wide(name)?;
        unsafe {
            check_win32(RegSetValueExW(
                self.0,
                name.as_ptr(),
                0,
                REG_DWORD,
                (&value as *const u32).cast(),
                4,
            ))
        }
    }

    fn string(&self, name: &str) -> Result<Option<OsString>> {
        let name = wide(name)?;
        for _ in 0..3 {
            let mut kind = 0;
            let mut bytes = 0;
            let status = unsafe {
                RegQueryValueExW(
                    self.0,
                    name.as_ptr(),
                    null(),
                    &mut kind,
                    null_mut(),
                    &mut bytes,
                )
            };
            if status == ERROR_FILE_NOT_FOUND {
                return Ok(None);
            }
            check_win32(status)?;
            if kind != REG_SZ || bytes % 2 != 0 {
                return Ok(None);
            }
            ensure!(bytes <= 1_048_576, "registry string is unexpectedly large");
            let mut value = vec![0u16; bytes as usize / 2 + 1];
            let status = unsafe {
                RegQueryValueExW(
                    self.0,
                    name.as_ptr(),
                    null(),
                    &mut kind,
                    value.as_mut_ptr().cast(),
                    &mut bytes,
                )
            };
            if status == ERROR_MORE_DATA {
                continue;
            }
            if status == ERROR_FILE_NOT_FOUND {
                return Ok(None);
            }
            check_win32(status)?;
            if kind != REG_SZ || bytes % 2 != 0 {
                return Ok(None);
            }
            value.truncate(bytes as usize / 2);
            if value.last() == Some(&0) {
                value.pop();
            }
            if value.contains(&0) {
                return Ok(None);
            }
            return Ok(Some(OsString::from_wide(&value)));
        }
        bail!("registry value changed while reading it")
    }

    fn remove_value(&self, name: &str) -> Result<()> {
        let name = wide(name)?;
        let status = unsafe { RegDeleteValueW(self.0, name.as_ptr()) };
        if status == ERROR_FILE_NOT_FOUND {
            Ok(())
        } else {
            check_win32(status)
        }
    }
}

impl Drop for RegKey {
    fn drop(&mut self) {
        unsafe {
            RegCloseKey(self.0);
        }
    }
}

fn delete_key(path: &str) -> Result<()> {
    let path = wide(path)?;
    let status = unsafe { RegDeleteKeyW(HKEY_CURRENT_USER, path.as_ptr()) };
    if matches!(status, ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND) {
        Ok(())
    } else {
        check_win32(status)
    }
}

// windows-sys intentionally exposes no COM interface wrappers. These layouts follow
// IShellLinkW in ShObjIdl_core.h and IPersistFile in objidl.h from the Windows SDK.
// Unused methods retain their pointer-sized vtable slots.
#[repr(C)]
struct ShellLinkVtable {
    unknown: IUnknown_Vtbl,
    get_path: unsafe extern "system" fn(
        *mut c_void,
        *mut u16,
        i32,
        *mut WIN32_FIND_DATAW,
        u32,
    ) -> HRESULT,
    unused_id_list_and_description: [usize; 3],
    set_description: unsafe extern "system" fn(*mut c_void, *const u16) -> HRESULT,
    get_working_directory: usize,
    set_working_directory: unsafe extern "system" fn(*mut c_void, *const u16) -> HRESULT,
    get_arguments: usize,
    set_arguments: unsafe extern "system" fn(*mut c_void, *const u16) -> HRESULT,
    unused_hotkey_and_show_command: [usize; 4],
    get_icon_location: usize,
    set_icon_location: unsafe extern "system" fn(*mut c_void, *const u16, i32) -> HRESULT,
    unused_relative_path_and_resolve: [usize; 2],
    set_path: unsafe extern "system" fn(*mut c_void, *const u16) -> HRESULT,
}

#[repr(C)]
struct PersistFileVtable {
    unknown: IUnknown_Vtbl,
    get_class_id: usize,
    is_dirty: usize,
    load: unsafe extern "system" fn(*mut c_void, *const u16, u32) -> HRESULT,
    save: unsafe extern "system" fn(*mut c_void, *const u16, i32) -> HRESULT,
    save_completed: usize,
    get_cur_file: usize,
}

struct ComApartment(bool);

impl ComApartment {
    fn new() -> Result<Self> {
        let status = unsafe { CoInitializeEx(null(), COINIT_APARTMENTTHREADED as u32) };
        if status == RPC_E_CHANGED_MODE {
            return Ok(Self(false));
        }
        check_hr(status)?;
        Ok(Self(true))
    }
}

impl Drop for ComApartment {
    fn drop(&mut self) {
        if self.0 {
            unsafe {
                CoUninitialize();
            }
        }
    }
}

struct ComObject(*mut c_void);

impl ComObject {
    unsafe fn vtable<T>(&self) -> &T {
        unsafe { &**(self.0 as *const *const T) }
    }

    fn shell_link() -> Result<Self> {
        const IID_SHELL_LINK_W: GUID = GUID::from_u128(0x000214f9_0000_0000_c000_000000000046);
        let mut object = null_mut();
        unsafe {
            check_hr(CoCreateInstance(
                &ShellLink,
                null_mut(),
                CLSCTX_INPROC_SERVER,
                &IID_SHELL_LINK_W,
                &mut object,
            ))?;
        }
        Ok(Self(object))
    }

    fn persist_file(&self) -> Result<Self> {
        const IID_PERSIST_FILE: GUID = GUID::from_u128(0x0000010b_0000_0000_c000_000000000046);
        let mut object = null_mut();
        unsafe {
            check_hr((self.vtable::<IUnknown_Vtbl>().QueryInterface)(
                self.0,
                &IID_PERSIST_FILE,
                &mut object,
            ))?;
        }
        Ok(Self(object))
    }
}

impl Drop for ComObject {
    fn drop(&mut self) {
        unsafe {
            (self.vtable::<IUnknown_Vtbl>().Release)(self.0);
        }
    }
}

fn write_shortcut(
    destination: &Path,
    target: &Path,
    directory: &Path,
    arguments: &OsStr,
) -> Result<()> {
    let _apartment = ComApartment::new()?;
    let link = ComObject::shell_link()?;
    let target = wide(target)?;
    let directory = wide(directory)?;
    let arguments = wide(arguments)?;
    let description = wide("Bucket Mount")?;
    let destination = wide(destination)?;
    unsafe {
        let table = link.vtable::<ShellLinkVtable>();
        check_hr((table.set_path)(link.0, target.as_ptr()))?;
        check_hr((table.set_working_directory)(link.0, directory.as_ptr()))?;
        check_hr((table.set_arguments)(link.0, arguments.as_ptr()))?;
        check_hr((table.set_description)(link.0, description.as_ptr()))?;
        check_hr((table.set_icon_location)(link.0, target.as_ptr(), 0))?;
        let file = link.persist_file()?;
        check_hr((file.vtable::<PersistFileVtable>().save)(
            file.0,
            destination.as_ptr(),
            1,
        ))
        .context("save Start menu shortcut")
    }
}

fn shortcut_matches(shortcut: &Path, app: &Path) -> Result<bool> {
    Ok(paths_match(&shortcut_target(shortcut)?, app.as_os_str()))
}

/// Read the target stored in an existing shortcut without resolving it.
pub(super) fn shortcut_target(shortcut: &Path) -> Result<OsString> {
    let _apartment = ComApartment::new()?;
    let link = ComObject::shell_link()?;
    let file = link.persist_file()?;
    let shortcut = wide(shortcut)?;
    let mut target = vec![0u16; 32768];
    unsafe {
        check_hr((file.vtable::<PersistFileVtable>().load)(
            file.0,
            shortcut.as_ptr(),
            STGM_READ,
        ))
        .context("inspect existing Start menu shortcut")?;
        check_hr((link.vtable::<ShellLinkVtable>().get_path)(
            link.0,
            target.as_mut_ptr(),
            target.len() as i32,
            null_mut(),
            SLGP_RAWPATH as u32,
        ))?;
    }
    let end = target
        .iter()
        .position(|&unit| unit == 0)
        .unwrap_or(target.len());
    Ok(OsString::from_wide(&target[..end]))
}

#[cfg(test)]
pub(super) struct TestRegistryCleanup(String);

#[cfg(test)]
impl TestRegistryCleanup {
    pub(super) fn new(root: &str) -> Self {
        let suffix = root
            .strip_prefix(r"Software\BucketMountTests\")
            .expect("test registry namespace");
        uuid::Uuid::parse_str(suffix).expect("test registry UUID");
        Self(root.into())
    }
}

#[cfg(test)]
impl Drop for TestRegistryCleanup {
    fn drop(&mut self) {
        for key in [
            format!(r"{}\Uninstall", self.0),
            format!(r"{}\Run", self.0),
            self.0.clone(),
        ] {
            let _ = delete_key(&key);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registration_round_trip_and_ownership_guards() -> Result<()> {
        // Production registry paths and the real Start menu are never used here.
        let temp = tempfile::tempdir()?;
        let root = format!(r"Software\BucketMountTests\{}", uuid::Uuid::new_v4());
        let registration = Registration {
            uninstall_key: format!(r"{root}\Uninstall"),
            run_key: format!(r"{root}\Run"),
            shortcut: temp.path().join("Bucket Mount.lnk"),
        };
        let _cleanup = TestRegistryCleanup::new(&root);
        let bin = temp.path().join("install with spaces");
        fs::create_dir(&bin)?;
        let app = bin.join("bucket-mount.exe");
        fs::write(&app, b"test application")?;
        let config = temp.path().join("config with spaces.toml");

        registration.install(&bin, &config, true)?;
        let target = shortcut_target(&registration.shortcut)?;
        ensure!(
            paths_match(&target, app.as_os_str()),
            "shortcut targets {target:?} instead of {:?}",
            app.as_os_str()
        );
        let key = RegKey::open(&registration.uninstall_key)?.unwrap();
        assert_eq!(
            key.string("DisplayName")?,
            Some(OsString::from("Bucket Mount"))
        );
        let mut expected = quote(bin.join("uninstall.exe").as_os_str());
        expected.push(" --uninstall --quiet");
        assert_eq!(key.string("QuietUninstallString")?, Some(expected));
        drop(key);
        let run = RegKey::open(&registration.run_key)?.unwrap();
        let startup = run.string(STARTUP_VALUE)?.unwrap();
        ensure!(command_matches(&startup, &app));
        let mut expected_startup = quote(app.as_os_str());
        expected_startup.push(" --config ");
        expected_startup.push(quote(config.as_os_str()));
        expected_startup.push(" run");
        assert_eq!(startup, expected_startup);
        assert!(registration.install(&bin, &config, false).is_err());
        assert_eq!(run.string(STARTUP_VALUE)?, Some(startup));
        registration.uninstall(&bin)?;
        assert!(!registration.shortcut.exists());
        assert!(RegKey::open(&registration.uninstall_key)?.is_none());
        assert_eq!(run.string(STARTUP_VALUE)?, None);

        registration.install(&bin, &config, false)?;
        assert_eq!(run.string(STARTUP_VALUE)?, None);
        registration.uninstall(&bin)?;

        registration.install(&bin, &config, true)?;
        let foreign = temp.path().join("other-app.exe");
        fs::write(&foreign, b"other application")?;
        let mut foreign_command = quote(foreign.as_os_str());
        foreign_command.push(" run");
        run.set_string(STARTUP_VALUE, &foreign_command)?;
        let key = RegKey::open(&registration.uninstall_key)?.unwrap();
        key.set_string("InstallLocation", temp.path().as_os_str())?;
        write_shortcut(
            &registration.shortcut,
            &foreign,
            temp.path(),
            OsStr::new(""),
        )?;
        registration.uninstall(&bin)?;
        assert_eq!(run.string(STARTUP_VALUE)?, Some(foreign_command));
        assert!(RegKey::open(&registration.uninstall_key)?.is_some());
        ensure!(shortcut_matches(&registration.shortcut, &foreign)?);
        assert!(registration.install(&bin, &config, true).is_err());
        ensure!(shortcut_matches(&registration.shortcut, &foreign)?);
        Ok(())
    }
}
