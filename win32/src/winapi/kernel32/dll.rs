use crate::{
    machine::{Emulator, Machine},
    pe,
    winapi::{self, builtin::BuiltinDLL, types::*, ImportSymbol},
};
use std::collections::HashMap;

const TRACE_CONTEXT: &'static str = "kernel32/dll";

// HMODULE is index+1 into kernel32::State::dlls.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub struct HMODULET;
pub type HMODULE = HANDLE<HMODULET>;

impl HMODULE {
    fn from_dll_index(index: usize) -> Self {
        return HMODULE::from_raw((index + 1) as u32);
    }

    pub fn to_dll_index(&self) -> Option<usize> {
        if self.is_null() {
            return None;
        }
        Some(self.raw as usize - 1)
    }
}

pub struct DLL {
    pub name: String,

    pub dll: pe::DLL,

    /// If present, DLL is one defined in winapi/...
    builtin: Option<&'static BuiltinDLL>,
}

impl DLL {
    fn resolve_from_pe(&self, sym: &ImportSymbol) -> Option<u32> {
        match *sym {
            ImportSymbol::Name(name) => self.dll.names.get(name).copied(),
            ImportSymbol::Ordinal(ord) => self.dll.ordinals.get(&ord).copied(),
        }
    }

    pub fn resolve_from_builtin(
        &mut self,
        sym: &ImportSymbol,
        register: impl FnOnce(Result<&'static crate::shims::Shim, String>) -> u32,
    ) -> Option<u32> {
        let builtin = self.builtin?;

        let export = match *sym {
            ImportSymbol::Name(name) => builtin
                .exports
                .iter()
                .find(|&export| export.shim.name == name),
            ImportSymbol::Ordinal(ord) => builtin
                .exports
                .iter()
                .find(|&export| export.ordinal == Some(ord as usize)),
        };

        let addr = match export {
            Some(export) => register(Ok(&export.shim)),
            None => {
                let name = format!("{}:{}", self.name, sym);
                log::warn!("unimplemented: {}", name);
                register(Err(name))
            }
        };

        match *sym {
            ImportSymbol::Name(name) => {
                self.dll.names.insert(name.to_string(), addr);
            }
            ImportSymbol::Ordinal(ord) => {
                self.dll.ordinals.insert(ord, addr);
            }
        }
        return Some(addr);
    }

    pub fn resolve(
        &mut self,
        sym: ImportSymbol,
        register: impl FnOnce(Result<&'static crate::shims::Shim, String>) -> u32,
    ) -> u32 {
        if let Some(addr) = self.resolve_from_pe(&sym) {
            return addr;
        }
        if let Some(addr) = self.resolve_from_builtin(&sym, register) {
            return addr;
        }
        log::warn!("failed to resolve {}:{}", self.name, sym);
        0
    }
}

#[win32_derive::dllexport]
pub fn GetModuleHandleA(machine: &mut Machine, lpModuleName: Option<&str>) -> HMODULE {
    let name = match lpModuleName {
        None => return HMODULE::from_raw(machine.state.kernel32.image_base),
        Some(name) => name,
    };

    let name = name.to_ascii_lowercase();

    if let Some(index) = machine
        .state
        .kernel32
        .dlls
        .iter()
        .position(|dll| dll.name == name)
    {
        return HMODULE::from_dll_index(index);
    }

    return HMODULE::null();
}

#[win32_derive::dllexport]
pub fn GetModuleHandleW(machine: &mut Machine, lpModuleName: Option<&Str16>) -> HMODULE {
    let ascii = lpModuleName.map(|str| str.to_string());
    GetModuleHandleA(machine, ascii.as_deref())
}

#[win32_derive::dllexport]
pub fn GetModuleHandleExW(
    machine: &mut Machine,
    dwFlags: u32,
    lpModuleName: Option<&Str16>,
    hModule: Option<&mut HMODULE>,
) -> bool {
    if dwFlags != 0 {
        unimplemented!("GetModuleHandleExW flags {dwFlags:x}");
    }
    let hMod = GetModuleHandleW(machine, lpModuleName);
    if let Some(out) = hModule {
        *out = hMod;
    }
    return !hMod.is_null();
}

#[win32_derive::dllexport]
pub fn LoadLibraryA(machine: &mut Machine, filename: Option<&str>) -> HMODULE {
    let mut filename = filename.unwrap().to_ascii_lowercase();

    // See if already loaded.
    if let Some(index) = machine
        .state
        .kernel32
        .dlls
        .iter()
        .position(|dll| dll.name == filename)
    {
        return HMODULE::from_dll_index(index);
    }

    if filename.starts_with("api-") {
        match winapi::apiset(&filename) {
            Some(name) => filename = name.to_string(),
            None => return HMODULE::null(),
        }
    }

    // Check if builtin.
    if let Some(builtin) = winapi::DLLS.iter().find(|&dll| dll.file_name == filename) {
        machine.state.kernel32.dlls.push(DLL {
            name: filename,
            dll: pe::DLL {
                names: HashMap::new(),
                ordinals: HashMap::new(),
                entry_point: 0,
            },
            builtin: Some(builtin),
        });
        return HMODULE::from_dll_index(machine.state.kernel32.dlls.len() - 1);
    }

    let mut file = machine.host.open(&filename);
    let mut contents = Vec::new();
    let mut buf: [u8; 16 << 10] = [0; 16 << 10];
    loop {
        let mut len = 0u32;
        assert!(file.read(&mut buf, &mut len));
        if len == 0 {
            break;
        }
        contents.extend_from_slice(&buf[..len as usize]);
    }
    // TODO: close file.
    if contents.len() == 0 {
        // HACK: zero-length indicates not found.
        return HMODULE::null();
    }

    let dll = pe::load_dll(machine, &filename, &contents).unwrap();
    machine.state.kernel32.dlls.push(DLL {
        name: filename,
        dll,
        builtin: None,
    });
    HMODULE::from_dll_index(machine.state.kernel32.dlls.len() - 1)
}

#[win32_derive::dllexport]
pub fn LoadLibraryExW(
    machine: &mut Machine,
    lpLibFileName: Option<&Str16>,
    hFile: HFILE,
    dwFlags: u32,
) -> HMODULE {
    let filename = lpLibFileName.map(|f| f.to_string());
    LoadLibraryA(machine, filename.as_deref())
}

/// The argument to GetProcAddress is an ImportSymbol stuffed into a u32.
#[derive(Debug)]
pub struct GetProcAddressArg<'a>(pub ImportSymbol<'a>);

impl<'a> winapi::stack_args::FromStack<'a> for GetProcAddressArg<'a> {
    unsafe fn from_stack(mem: memory::Mem<'a>, sp: u32) -> Self {
        let lpProcName = <u32>::from_stack(mem, sp);
        if lpProcName & 0xFFFF_0000 == 0 {
            GetProcAddressArg(ImportSymbol::Ordinal(lpProcName))
        } else {
            let proc_name = mem.slicez(lpProcName).unwrap().to_ascii();
            GetProcAddressArg(ImportSymbol::Name(proc_name))
        }
    }
}

#[win32_derive::dllexport]
pub fn GetProcAddress(
    machine: &mut Machine,
    hModule: HMODULE,
    lpProcName: GetProcAddressArg,
) -> u32 {
    let index = hModule.to_dll_index().unwrap();
    if let Some(dll) = machine.state.kernel32.dlls.get_mut(index) {
        return dll.resolve(lpProcName.0, |shim| machine.emu.register(shim));
    }
    log::error!("GetProcAddress({:x?}, {:?})", hModule, lpProcName);
    0 // fail
}
