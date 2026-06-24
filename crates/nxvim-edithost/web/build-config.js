// Build-time feature flags for the web edit-host. The committed default is the **standard
// editor** (no local in-browser process host). The python-demo build (build-demo.sh) ships a
// copy of this file with `localHost: true`, which makes the Worker install the local Pyodide
// host (web/local-host.mjs) so a serverless `:terminal python …` runs CPython in-browser. The
// standard build never even imports local-host.mjs — see worker.mjs's boot. Keep this a plain,
// dependency-free ESM constant so worker.mjs can statically import it.
export const BUILD = {
  localHost: false,
};
