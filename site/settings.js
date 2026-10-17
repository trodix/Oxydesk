// Each entry: key (config key and long option), cat, type, def (default), modes, where, desc (what it is for).
const CONF = "rdsktp.conf, a drop-in, or the command line";
const SETTINGS = [
 // Mode
 {key:"login", cat:"Mode", type:"yes | (flag)", def:"yes (the default mode)", modes:"—", where:CONF, desc:"Remote login: the client types a Linux account and password, checked by PAM through rdsktp-authd, and gets its own KDE Plasma session. One real session per user, TLS only (no NLA). Needs rdsktp-authd.service."},
 {key:"session", cat:"Mode", type:"kwin", def:"unset", modes:"—", where:CONF, desc:"Serve one virtual KWin session of the user running the server (captured and controlled) instead of per-user logins. Needs a build with the kwin feature."},
 {key:"test-mode", cat:"Mode", type:"yes | (flag)", def:"unset", modes:"—", where:CONF, desc:"Synthetic desktop (test pattern, echo of keyboard and clipboard). For tests only, not a usable desktop. Exactly one of login, session, test-mode is used."},
 {key:"config", cat:"Mode", type:"path", def:"/etc/rdsktp/rdsktp.conf (if present) plus FILE.d/*.conf", modes:"all", where:"command line only (--config FILE)", desc:"Read another configuration file instead of the default one. The command line still wins over it."},
 // Network
 {key:"listen", cat:"Network", type:"ADDR:PORT", def:"0.0.0.0:3389", modes:"all", where:CONF, desc:"Address and port to listen on. Use 127.0.0.1:3389 to accept local connections only (for example behind an SSH tunnel or VPN), or a port above 1024 to move away from the RDP default."},
 {key:"state-dir", cat:"Network", type:"path", def:"/var/lib/rdsktp", modes:"all", where:CONF, desc:"Directory of the self-signed TLS certificate (tls-cert.pem, tls-key.pem), generated at the first start. Replace both files there to use your own certificate."},
 {key:"authd-socket", cat:"Network", type:"path", def:"/run/rdsktp/authd.sock", modes:"login", where:CONF, desc:"Socket of rdsktp-authd, the only privileged component. Change it only if you also moved the authd socket (see rdsktp-authd below)."},
 // Display
 {key:"width", cat:"Display", type:"pixels", def:"1920", modes:"test-mode, session", where:CONF, desc:"Width of the desktop. The client's window size is followed when it can be."},
 {key:"height", cat:"Display", type:"pixels", def:"1080", modes:"test-mode, session", where:CONF, desc:"Height of the desktop. The client's window size is followed when it can be."},
 {key:"outputs", cat:"Display", type:"1 to 16", def:"1", modes:"all", where:CONF, desc:"Number of monitors offered, laid out side by side. In login mode this is only a default: the session follows the monitors the client announces (mstsc /multimon, up to 16)."},
 {key:"fps", cat:"Display", type:"number", def:"30", modes:"all", where:CONF, desc:"Maximum number of frames per second captured and sent. Lower it to save bandwidth and CPU, raise it for smoother motion."},
 // Video
 {key:"encoder", cat:"Video", type:"auto | vaapi | openh264", def:"auto", modes:"all", where:CONF, desc:"H.264 encoder. auto uses the GPU through VA-API when usable, else software OpenH264. vaapi forces the GPU, openh264 forces software (useful on a VM without a GPU, or to rule out a driver problem)."},
 {key:"dmabuf", cat:"Video", type:"auto | on | off", def:"auto", modes:"all", where:CONF, desc:"Offer DMA-BUF capture buffers (zero-copy with a GPU encoder). auto enables them only with a GPU encoder. Turn off if capture shows corruption with a GPU."},
 {key:"adaptive-bitrate", cat:"Video", type:"on | off", def:"on", modes:"all", where:CONF, desc:"Bitrate and number of frames in flight follow what the client's link carries. Turn off to keep a fixed bitrate on a link you know is good."},
 // Audio
 {key:"audio", cat:"Audio", type:"pulse | tone | off", def:"pulse with a KWin session, tone in test-mode", modes:"session, test-mode", where:CONF, desc:"pulse captures the session's sound through PipeWire/PulseAudio. tone plays a test tone (diagnostics). off disables audio."},
 // Authentication
 {key:"nla-user", cat:"Authentication", type:"account name", def:"unset (NLA off)", modes:"session, test-mode (incompatible with login)", where:CONF + "; the password only through the RDSKTP_PASSWORD environment variable", desc:"Enables NLA (network level authentication) for this single account name. The password is never in the configuration file: set RDSKTP_PASSWORD in the service environment (a drop-in of the unit, mode 0600)."},
 {key:"max-login-failures", cat:"Authentication", type:"number", def:"5", modes:"login", where:CONF, desc:"Failed login attempts from one client address before that address is blocked for login-lockout-secs. Protects against password guessing."},
 {key:"max-user-failures", cat:"Authentication", type:"number", def:"10", modes:"login", where:CONF, desc:"Failed login attempts on one account (from any address) before the account is blocked for login-lockout-secs."},
 {key:"login-lockout-secs", cat:"Authentication", type:"seconds", def:"300", modes:"login", where:CONF, desc:"How long an address or account stays blocked after too many failures."},
 // Reconnection
 {key:"reconnect-timeout", cat:"Reconnection", type:"seconds", def:"10 (0 turns it off)", modes:"login", where:CONF, desc:"After a network loss, mstsc and FreeRDP resume their session by themselves, without the password, if they come back within this time. The cookie they present is single use and valid only from the same address; a client that disconnects on purpose gets none. The session and its applications are kept."},
 {key:"dead-peer-timeout", cat:"Reconnection", type:"seconds", def:"8 (0 leaves it to the kernel, which takes minutes)", modes:"all", where:CONF, desc:"A client that stops answering (network cut without a clean close) is dropped after this time. This frees the server so the client can reconnect."},
 // KWin session
 {key:"kwin-socket", cat:"KWin session", type:"name", def:"rdsktp-0", modes:"session", where:CONF, desc:"Name of the Wayland socket of the virtual KWin started for the session."},
 {key:"session-app", cat:"KWin session", type:"command and arguments", def:"none", modes:"session", where:CONF + " (repeat the line for several programs)", desc:"Program started in the session, with its arguments, for example plasmashell or konsole. A list given on the command line replaces the whole list from the file."},
 // Redirections
 {key:"drives-dir", cat:"Redirections", type:"path | off", def:"$XDG_RUNTIME_DIR/rdsktp-drives with a KWin session; off with test-mode", modes:"all", where:CONF, desc:"Drive redirection: folders shared by the client are mounted (FUSE) under DIR/<name> and appear as ordinary read-write folders. off disables it. Needs fuse3."},
 {key:"printers", cat:"Redirections", type:"path | off", def:"$XDG_RUNTIME_DIR/rdsktp-printers with a KWin session", modes:"all", where:CONF, desc:"Printer redirection: one socket per client printer in this directory (any script can print through it). off disables it."},
 {key:"printers-cups", cat:"Redirections", type:"on | off", def:"on when CUPS answers", modes:"all", where:CONF, desc:"Also create a CUPS queue per client printer, so LibreOffice, Firefox or lp can print to it. Queues are named USER-PRINTER and visible only to the session user. Needs the user to be in the CUPS admin group (lpadmin)."},
 {key:"printer-mode", cat:"Redirections", type:"auto | raw | raw:ps|pcl|text | xps, per printer PATTERN=MODE", def:"auto", modes:"all", where:CONF, desc:"Format of the jobs sent to client printers. auto sends raw jobs when the driver name states a language (PostScript, PCL, text), XPS otherwise if the client accepts it, else the printer is refused. Per printer: comma-separated PATTERN=MODE where * matches anything, e.g. xps,HP*=raw:pcl. XPS needs ghostscript."},
 // authd
 {key:"--socket", cat:"rdsktp-authd", type:"path", def:"/run/rdsktp/authd.sock", modes:"login", where:"command line of rdsktp-authd (override ExecStart= in a systemd drop-in)", desc:"Socket on which the privileged authentication helper listens. Must match authd-socket of rdsktp."},
 {key:"--pam-service", cat:"rdsktp-authd", type:"name", def:"rdsktp", modes:"login", where:"command line of rdsktp-authd", desc:"PAM service used to check the credentials. The package installs /etc/pam.d/rdsktp, which includes the system's common-auth and common-account. Edit it to add, for example, two-factor authentication."},
 {key:"--min-uid", cat:"rdsktp-authd", type:"number", def:"1000", modes:"login", where:"command line of rdsktp-authd", desc:"Lowest user id allowed to log in remotely. Keeps system accounts (root and service users) out of reach."},
 // Environment
 {key:"RDSKTP_PASSWORD", cat:"Environment", type:"string", def:"unset", modes:"nla-user", where:"environment of the service (a systemd drop-in with Environment=, file mode 0600)", desc:"The NLA password for nla-user. Read only from the environment so that it never ends up in a configuration file or the process list."},
 {key:"RUST_LOG", cat:"Environment", type:"filter", def:"info", modes:"all", where:"environment of rdsktp, rdsktp-authd and the agent", desc:"Log verbosity (tracing filter), for example RUST_LOG=debug or RUST_LOG=rdsktp_rdp=debug. Read the logs with journalctl -u rdsktp -u rdsktp-authd."},
 {key:"RDSKTP_KEYBOARD_LAYOUT", cat:"Environment", type:"off", def:"unset", modes:"all", where:"environment of the session user", desc:"Set to off to stop adopting the client's keyboard layout, for a user who shares the D-Bus session of a real desktop."},
 {key:"RDSKTP_POINTER", cat:"Environment", type:"hidden", def:"unset (cursor shape sent)", modes:"session", where:"environment of the session user", desc:"Set to hidden to not capture the mouse pointer shape in the video stream."},
 // Files
 {key:"/etc/rdsktp/rdsktp.conf", cat:"Files", type:"file", def:"installed by the package", modes:"all", where:"edit it (kept on upgrades)", desc:"Main configuration file, fully commented. Prefer a drop-in in /etc/rdsktp/rdsktp.conf.d/ for local changes."},
 {key:"/etc/pam.d/rdsktp", cat:"Files", type:"file", def:"includes common-auth and common-account", modes:"login", where:"edit it", desc:"PAM stack used to check remote logins. Changes apply to the next login."},
 {key:"/var/lib/rdsktp/", cat:"Files", type:"directory", def:"created at start", modes:"all", where:"state-dir", desc:"Holds the self-signed TLS certificate and key (tls-cert.pem, tls-key.pem)."}
];

const esc = s => String(s).replace(/[&<>"]/g, c => ({"&":"&amp;","<":"&lt;",">":"&gt;",'"':"&quot;"}[c]));
const anchor = key => esc(key.replace(/[^a-z0-9_-]/gi, "_"));

function hl(text, terms) {
  let out = esc(text);
  for (const t of terms) { if (!t) continue; out = out.replace(new RegExp("(" + t.replace(/[.*+?^${}()|[\]\\]/g, "\\$&") + ")(?![^<]*>)", "gi"), "<mark>$1</mark>"); }
  return out;
}

// One setting as an HTML card. Shared by the page (live search) and ci/prerender-docs.mjs (static HTML for crawlers).
function card(p, terms = []) {
  const plain = p.cat === "Environment" || p.cat === "Files" || p.key.startsWith("--");
  const flag = plain ? p.key : "--" + p.key;
  return `<article class="param" id="${anchor(p.key)}">
    <h3><a href="#${anchor(p.key)}"><code>${hl(p.cat === "Files" || p.cat === "Environment" ? p.key : p.key.replace(/^--/, ""), terms)}</code></a><span class="cat">${esc(p.cat)}</span></h3>
    <p>${hl(p.desc, terms)}</p>
    <div class="meta">
      <span><b>Values:</b> ${hl(p.type, terms)}</span>
      <span><b>Default:</b> ${hl(p.def, terms)}</span>
      <span><b>Applies to:</b> ${hl(p.modes, terms)}</span>
      <span><b>Set it in:</b> ${hl(p.where, terms)}${plain ? "" : " (as <code>" + esc(flag) + "</code>)"}</span>
    </div></article>`;
}

if (typeof module !== "undefined") module.exports = { SETTINGS, card };
