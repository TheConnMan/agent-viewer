The `termux/termux-app` repository is released under [GPLv3 only](https://www.gnu.org/licenses/gpl-3.0.html) license.

### Exceptions

- [Terminal Emulator for Android](https://github.com/jackpal/Android-Terminal-Emulator) code is used which is released under [Apache License 2.0](https://www.apache.org/licenses/LICENSE-2.0). Check [`terminal-view`](terminal-view) and [`terminal-emulator`](terminal-emulator) libraries.
- Check [`termux-shared/LICENSE.md`](termux-shared/LICENSE.md) for `termux-shared` library related exceptions.

## Vendored copy

This module is the `terminal-emulator` and `terminal-view` Java sources of
[`termux/termux-app`](https://github.com/termux/termux-app), flattened into a single Gradle
library (`com.termux.terminal` and `com.termux.view` share one source set here).

- Upstream tag: **v0.118.3**
- Upstream commit: **5b657c6adf4304e5198951ce815fe0205dcac29c**
- Previously vendored from: v0.118.0 (`6e2689f55295fa444be8ac8592c527c2c5ef3253`)

One commit is cherry-picked on top of the tag:

- [`30ebb2de`](https://github.com/termux/termux-app/commit/30ebb2dee381d292ade0f2868cfde0f9f20b89fe)
  — `KeyHandler`: fix the inverted `kP`/`kN` termcap values for `KEYCODE_PAGE_UP`/`KEYCODE_PAGE_DOWN`.

### Local modifications

Everything not listed below is a verbatim copy of the upstream file at that tag. Keep it that way
so the next re-vendor stays a diff, not a rewrite.

- **`JNI.java` is not vendored.** The phone never forks a local shell; the process lives on the
  dev box and its PTY bytes arrive over a WebSocket. Vendoring the JNI declarations would drag in
  `src/main/jni/termux.c` and an NDK build for code that can never run here.
- **`TerminalSession.java` is modified** to match, and is the only upstream file with local edits:
  - `final` dropped from the class so `RemoteTerminalSession` can extend it.
  - `initializeEmulator` throws `UnsupportedOperationException` instead of calling
    `JNI.createSubprocess` and starting the reader/writer/waiter threads.
  - `updateSize` no longer calls `JNI.setPtyWindowSize`.
  - `finishIfRunning` no longer sends `SIGKILL`; `cleanupResources` no longer calls `JNI.close`.
  - `wrapFileDescriptor`, `mTerminalFileDescriptor`, and the now-unused `android.system.*` and
    `java.io.File*` imports are removed.
- **`RemoteTerminalSession.java` and `RemoteTerminalTransport.java` are ours**, added in package
  `com.termux.terminal` because `TerminalSession#mEmulator` is package private. They are not
  upstream files and have no upstream counterpart.
