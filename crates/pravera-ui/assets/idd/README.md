# Bundled VirtualDisplayDriver

The `MttVDD` Indirect Display Driver package (x64, release 24.12.24), from
the `VirtualDrivers/Virtual-Display-Driver` releases
(`Signed-Driver-v24.12.24-x64.zip`). It is Authenticode-signed by
**SignPath Foundation** — not Microsoft/WHQL-signed — so Windows treats its
publisher as untrusted until told otherwise.

These four files are embedded into `pravera.exe` at build time
(`include_bytes!` in `crates/pravera-capture/src/idd.rs`), so a portable
install needs nothing next to the executable. This directory is the source
copy the build embeds; do not delete it.

## What Pravera does with it

Only on a machine with no real monitor (decided from each monitor's EDID:
the driver's own `MTT1337` and Windows' `Default_Monitor` placeholder don't
count), and only from an elevated process (the service's agent, or a run as
administrator), Pravera:

1. Lays the package out in `%ProgramData%\Pravera\idd`.
2. Adds the catalogue's signing certificate (only the signer, not its chain)
   to the machine's **Trusted Publishers** store. The upstream installer does
   the same thing. Without it, the first install waits on a "trust this
   publisher?" prompt that nobody on a headless box will answer.
3. Validates the driver's `vdd_settings.xml`. It reads the path from
   `HKLM\SOFTWARE\MikeTheTech\VirtualDisplayDriver\VDDPATH`, defaulting to
   `C:\VirtualDisplayDriver`. The driver parses that file with no error
   handling, so a malformed one means a monitor with no modes, or a crash.
   Pravera rewrites it atomically when it wouldn't serve 1920x1080 @ 60 Hz.
4. Creates the root device node (`ROOT\MTTVDD\000n`, hardware id
   `Root\MttVDD`) and installs the driver on it, non-interactively. A node
   left by the upstream installer (`ROOT\DISPLAY\000n`) is adopted rather
   than duplicated.
5. Waits for the monitor to reach the desktop, then makes it primary when
   the only other displays are placeholders.

On a desktop with real monitors nothing happens automatically. The Settings
card's buttons do the same steps when somebody asks.

Security note: step 2 trusts that exact certificate machine-wide. SignPath
Foundation signs releases for many open-source projects, so any driver
signed with the same certificate would then install on that machine without
a prompt. To undo it, remove the certificate from `certlm.msc` → Trusted
Publishers.
