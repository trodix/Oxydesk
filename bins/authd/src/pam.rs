//! Authentification PAM par `dlopen` de `libpam.so.0` (pas d'en-têtes ni de crate `pam` requis).

use std::ffi::{c_char, c_int, c_void, CString};
use std::ptr;

const PAM_SUCCESS: c_int = 0;
const PAM_PROMPT_ECHO_OFF: c_int = 1;
const PAM_PROMPT_ECHO_ON: c_int = 2;
const PAM_CONV_ERR: c_int = 19;
const PAM_DISALLOW_NULL_AUTHTOK: c_int = 1;

#[repr(C)]
struct PamMessage {
    msg_style: c_int,
    msg: *const c_char,
}

#[repr(C)]
struct PamResponse {
    resp: *mut c_char,
    resp_retcode: c_int,
}

type ConvFn = unsafe extern "C" fn(c_int, *mut *const PamMessage, *mut *mut PamResponse, *mut c_void) -> c_int;

#[repr(C)]
struct PamConv {
    conv: ConvFn,
    appdata_ptr: *mut c_void,
}

/// Réponse unique (mot de passe ou nom) à toute invite PAM.
unsafe extern "C" fn conversation(
    n: c_int,
    msgs: *mut *const PamMessage,
    resp: *mut *mut PamResponse,
    appdata: *mut c_void,
) -> c_int {
    if n <= 0 || msgs.is_null() || resp.is_null() {
        return PAM_CONV_ERR;
    }
    let secret = appdata as *const c_char;
    let out = libc::calloc(n as usize, std::mem::size_of::<PamResponse>()) as *mut PamResponse;
    if out.is_null() {
        return PAM_CONV_ERR;
    }
    for i in 0..n as isize {
        let style = (**msgs.offset(i)).msg_style;
        if style == PAM_PROMPT_ECHO_OFF || style == PAM_PROMPT_ECHO_ON {
            (*out.offset(i)).resp = libc::strdup(secret);
        }
    }
    *resp = out;
    PAM_SUCCESS
}

pub struct Pam {
    lib: libloading::Library,
    service: CString,
}

#[derive(Debug, PartialEq, Eq)]
pub enum PamOutcome {
    Granted,
    Denied,
}

impl Pam {
    pub fn load(service: &str) -> anyhow::Result<Self> {
        // SAFETY: chargement d'une bibliothèque système standard.
        let lib = unsafe { libloading::Library::new("libpam.so.0") }?;
        Ok(Self { lib, service: CString::new(service)? })
    }

    /// Authentifie et vérifie le compte (`pam_authenticate` + `pam_acct_mgmt`). Aucune session PAM n'est ouverte ici :
    /// c'est systemd (`PAMName=` de l'unité de l'agent) qui ouvre la session logind.
    pub fn authenticate(&self, username: &str, password: &str) -> anyhow::Result<PamOutcome> {
        let (Ok(user), Ok(pass)) = (CString::new(username), CString::new(password)) else {
            return Ok(PamOutcome::Denied); // octet NUL : jamais valide
        };
        type Start = unsafe extern "C" fn(*const c_char, *const c_char, *const PamConv, *mut *mut c_void) -> c_int;
        type Auth = unsafe extern "C" fn(*mut c_void, c_int) -> c_int;
        type End = unsafe extern "C" fn(*mut c_void, c_int) -> c_int;
        // SAFETY: signatures conformes à <security/pam_appl.h> ; `conv` et `pass` vivent jusqu'à `pam_end`.
        unsafe {
            let start: libloading::Symbol<Start> = self.lib.get(b"pam_start\0")?;
            let authenticate: libloading::Symbol<Auth> = self.lib.get(b"pam_authenticate\0")?;
            let acct: libloading::Symbol<Auth> = self.lib.get(b"pam_acct_mgmt\0")?;
            let end: libloading::Symbol<End> = self.lib.get(b"pam_end\0")?;
            let conv = PamConv { conv: conversation, appdata_ptr: pass.as_ptr() as *mut c_void };
            let mut handle: *mut c_void = ptr::null_mut();
            let rc = start(self.service.as_ptr(), user.as_ptr(), &conv, &mut handle);
            if rc != PAM_SUCCESS {
                anyhow::bail!("pam_start a échoué ({rc})");
            }
            let mut rc = authenticate(handle, PAM_DISALLOW_NULL_AUTHTOK);
            if rc == PAM_SUCCESS {
                rc = acct(handle, 0);
            }
            end(handle, rc);
            Ok(if rc == PAM_SUCCESS { PamOutcome::Granted } else { PamOutcome::Denied })
        }
    }
}
