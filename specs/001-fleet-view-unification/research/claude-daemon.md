# Claude Code daemon protocol research

Question: can `agent-viewer` (a) attach to a live Claude Code session **in place** by speaking
the daemon's socket protocol directly, instead of forking a `claude attach` child, and (b)
perform a **real** session rename through the daemon?

Investigated 2026-07-26 against Claude Code **2.1.220** on this box.

## Verdicts

| Question | Verdict |
| --- | --- |
| In-place PTY attach (bidirectional bytes + resize) | **ACHIEVABLE** — verified working from a non-`claude` process |
| Native rename through the daemon | **NOT-PRACTICAL** — no rename operation exists on any daemon socket |
| `claude rm` deletes a git worktree | **Yes, conditionally** — heavily guarded, refuses on dirty/unpushed/locked |

---

## How the evidence was gathered

`claude` is an ELF Bun single-file executable with the JavaScript bundle embedded as
**plaintext**, so it is greppable directly:

```
$ readlink -f $(which claude)
/home/theconnman/.local/share/claude/versions/2.1.220
$ file /home/theconnman/.local/share/claude/versions/2.1.220
... ELF 64-bit LSB executable, x86-64 ... not stripped
```

Extraction used for all quotes below (byte offsets cited against the binary are stable for
this build; re-derive with `grep -abo -F '<snippet>' <binary>`):

```bash
B=/home/theconnman/.local/share/claude/versions/2.1.220
dd if=$B bs=1M skip=230 count=32 2>/dev/null | tr -c '[:print:]\n\t' '\n' > /tmp/cc_js.txt
```

Live probes were run from Python against the real daemon at
`/tmp/cc-daemon-1000/13512c9a/`. Two throwaway sessions (`0b5dab67`, `864d8566`) were created
with `claude --bg` and removed with `claude rm` afterwards; the roster is back to only the
user's own session.

---

## 1. The three sockets, and what each one actually is

| Socket | Server is | Client is | Framing |
| --- | --- | --- | --- |
| `control.sock` (per daemon instance) | the **daemon** | any CLI (`claude attach`, `claude agents`, …) | newline-delimited JSON, then **raw bytes** after `op:"attach"` |
| `ptySock` (per worker) | the **pty host** process (`claude --bg-pty-host`) | the daemon (and anyone else) | **length-prefixed binary frames** |
| `rendezvousSock` (per worker) | the **worker's own Claude REPL** | the daemon supervisor | newline-delimited JSON |

The repo's current rename attempt targets the wrong socket **and** a nonexistent message
type — see section 4.

---

## 2. In-place PTY attach — ACHIEVABLE

There are two independent ways in, and **both were verified working from Python**.

### 2a. Path A (recommended): `op:"attach"` on `control.sock`

This is exactly what `claude attach` does. The daemon's control-socket dispatcher is at binary
offset ~`262850650`. Request schema (zod `hks`, at offset ~`254352000` region — quoted verbatim):

```js
v.object({proto:t,op:v.literal("attach"),short:e,auth:v.string().optional(),
  cols:v.number().int().min(1).max(VBe),rows:v.number().int().min(1).max(VBe),
  attachId:v.string().optional(),
  caps:v.object({terminal:v.string().nullable(),mux:v.enum(["tmux","screen","zellij"]).nullable(),
    ssh:v.boolean(),wheelFlood:...,hyperlinks:...,progressReporting:...,wtSession:...,
    isVscodeTerm:...,browser:v.string().nullable().optional(),
    colorLevel:v.union([v.literal(0),v.literal(1),v.literal(2),v.literal(3)]).optional(),
    syncOutput:...,editor:...,systemTheme:v.enum(["dark","light"]).optional()}).optional(),
  holdingFrame:v.boolean().optional()})
```

with `e = /^[a-f0-9]{8}$/` (the short id), `VBe = 1e4`, and `t` the proto version (see §5).

Auth is the daemon control key, read as a trimmed utf8 string from
`~/.claude/daemon/control.key` (`function oks()` at offset `254337191`):

```js
function oks(){return vb.join(QHe(),"control.key")}
async function lad(){ ... let t=Ddr.randomBytes(16).toString("hex"); ... }
```

so it is **16 random bytes rendered as 32 hex chars**, not 32 raw bytes. Verified: the file is
32 bytes on disk and is a hex string.

Auth is currently *optional* — the daemon falls back to peer-uid checking with a warning:

```js
case"attach":{if(A.auth===void 0){if(!gcf)gcf=!0,O("tengu_dead_probe_bg_attach_noauth",{});
  w("[bg-attach] legacy client (no control key) — allowed via peerUid",{level:"warn"})}
  else if(!sIe(A.auth,m))return Th(t,{ok:!1,error:"attach rejected: the presented daemon control key doesn't match ...",code:"EAUTH"});
```

The telemetry name `tengu_dead_probe_bg_attach_noauth` says Anthropic is measuring the no-auth
path with intent to remove it. **Always send `auth`.**

After the daemon replies with one JSON line, **the same socket becomes a raw byte pipe** in both
directions — output via `t.write(ee)` on the `onStream` subscription, input via:

```js
let se=new Scf.StringDecoder("utf8"),ne=(ee)=>{let te=se.write(ee);
  if(te.length>0&&!Vlf(te))b.lastInputAttacher=re;b.write(te)};
if(n.length)ne(n);t.on("data",ne),
```

Detach is simply closing the socket; the daemon's `t.once("close",...)` handler removes the
attacher, restores focus (`b.seedFocus(!1)`) and clears caps (`b.sendAttacherCaps(null)`).

**Multiple simultaneous attachers are supported on Linux** (`b.attachers` is a `Map`; the
`kick()` eviction loop is guarded by `if(Mt()==="windows")`). Attaching therefore does not
displace a human already in `claude attach`.

Resize is a *separate* short-lived connection with `op:"resize"` carrying the same `attachId`:

```js
case"resize":{let b=o.get(A.short); ... if(A.attachId){let T=b.attachers.get(A.attachId);
  ... if(T.cols=A.cols,T.rows=A.rows,T.repaint)return T.repaint(), ...
```

**Live proof** (Python, non-`claude` process, against a real running session `864d8566`):

```
ATTACH HDR: {"ok":true,"op":"attach","decModes":[1000,1002,1003,1006,2004,1004,2031],
             "via":"spare","tempo":"idle","state":"working","cached":false,"stale":false}
ATTACH raw bytes received: 9777
ATTACH sample: b'\x1b[?2026h\x1b[?25l\x1b[2J\x1b[H\r\x1b[1B\x1b[38;2;215;119;87m...Claude Code...v2.1.220...'
```

Note the reply hands back `decModes` — the same array already present in the roster — so the
TUI knows which DEC private modes to enable for mouse/bracketed-paste passthrough.

Also verified read-only on the same socket:

```
LIST ok= True njobs= 1
   0214d552 working ...
```

### 2b. Path B: direct framed connection to `ptySock`

The pty host (`claude --bg-pty-host <sock> <cols> <rows> -- <file> [args...]`, offset
`259934527`) serves a **length-prefixed binary framing**:

```js
function Gnn(e){let t=...;let r=Buffer.allocUnsafe(D1t+t.length);
  return r.writeUInt32BE(t.length,0),r.writeUInt8(P1t,4),t.copy(r,D1t),r}   // offset 254346603
function x4(e){let t=Buffer.from(Ie(e),"utf8");let r=Buffer.allocUnsafe(D1t+t.length);
  return r.writeUInt32BE(t.length,0),r.writeUInt8(Unn,4),t.copy(r,D1t),r}   // offset 254346775
var P1t=0,Unn=1,jnn=262144,D1t=5,Odr=1048576,VBe=1e4;
```

So: **5-byte header = uint32BE payload length + uint8 kind**, kind `0` = raw PTY bytes,
kind `1` = JSON control. Max frame 1 MiB; scrollback ring is 256 KiB (`jnn`).

Server behavior on connect (offset `259934527` region):
1. sends `{"t":"hello","replPid":N,"version":"2.1.220"}`
2. replays the whole ring buffer as kind-0 frames
3. sends `{"t":"live"}` then `{"t":"ping"}`
4. client **must** answer `{"t":"pong"}` or it is destroyed after 3 missed 60s pings.

Input gating:

```js
if(j.kind===P1t){if(c&&!m.has(F)){if(!g.has(F))g.add(F),L(F,x4({t:"auth-required"}));return}
  ...R.write(j.payload)...}
else if(j.kind===Unn)if(j.ctrl.t==="pong"){...}
else if(j.ctrl.t==="auth"){if(sIe(j.ctrl.token,c))m.add(F)} else P(j.ctrl)
```

- **Output requires no auth at all** — the host broadcasts to every connected socket.
- **Input (kind-0 DATA) requires** `{"t":"auth","token":<roster.ptyAuth>}` first.
- **`resize` and `kill` are NOT auth-gated** (they fall through to `P(j.ctrl)` before the auth
  check). `resize` bounds are `0 < cols,rows <= 10000`.

**Live proof** (Python, direct to `/tmp/cc-daemon-1000/13512c9a/spare/e482c213.pty.sock`):

```
PTY frames: [(1, '{"t":"hello","replPid":153085,"version":"2.1.220"}'), (0, 13), (0, 53),
             (0, 24), (0, 21), (0, 4469), (0, 91), (0, 2181)]
PTY total frames: 190 data bytes: 32579
```

`ptyAuth` in the roster is 32 hex chars (`_hn.randomBytes(16).toString("hex")`).

### Which path to prefer

**Path A (`control.sock` attach).** Reasons:
- It is the daemon's own supported client surface with an explicit proto-version gate (§5),
  so breakage is *announced* rather than silent.
- The daemon handles startup holding-frames, stall detection/respawn, `attacher-caps`
  propagation, focus seeding, and the `decModes` snapshot for you. Path B gets none of that.
- Path B bypasses the daemon's attacher accounting entirely; the session would never learn a
  human is watching (no repaint, no colorLevel/theme negotiation).
- Path B's ctrl channel exposes an unauthenticated `kill` — a footgun to build a hotkey near.

---

## 3. The rendezvous socket — and a trap

`rendezvousSock` is served by the **worker's own Claude REPL**, not the daemon
(`startRendezvousServer`, around offset `254481350`). Its handled message types are exhaustively:

```js
if(r.type==="shutdown"){YOy();return}
if(r.type==="repaint"){XOy();return}
if(r.type==="attacher-caps"){JOy(r);return}
if(r.type==="reply"&&typeof r.text==="string")QOy(r)
```

Auth is the **first frame containing a `role` key**, and the daemon sends:

```js
p.write(Ie({proto:Um,role:"supervisor",supervisorPid:process.pid,auth:o})+`\n`)   // offset 259959423
```

Unauthenticated messages are dropped with `{"type":"reply-rejected"}` — which is exactly the
reply `crates/agent-viewer-core/src/claude.rs` observes today:

```js
if(dpr&&!bon&&r.type!=="repaint"){w(`[bg-rv] dropped ${...} from un-authed connection`,{level:"warn"});
  if(r.type!=="attacher-caps")ycd();ate({type:"reply-rejected"});return}
```

### Trap: connecting to `rendezvousSock` evicts the daemon

```js
tKe=_cd.createServer((r)=>{jEe?.destroy(),jEe=r,bon=!1,eTo=0,fpr=!1, ...
```

The rv server keeps **exactly one** connection and destroys the previous one on every new
connect. Any agent-viewer connection to `rendezvousSock` therefore **kicks the daemon's
supervisor link off**, breaking heartbeat/state reporting until it reconnects. `agent-viewer`
should not touch this socket at all — including for the current rename attempt, which is
already doing this on every rename keypress.

---

## 4. Native rename — NOT-PRACTICAL

There is **no rename operation on any daemon surface**. The complete `op` union accepted by
`control.sock` (zod `hks`, quoted from the bundle) is:

```
ping, nudge, yield, lease, leases, await-ack, dispatch, list, has, kill, reply,
subscribe, attach, resize, ensure-spare, permission-response, respawn-stale, shutdown
```

and the dispatcher's fallthrough is `default:return Th(t,{ok:!1,error:`unknown op: ${A.op}`,code:"EUNKNOWN"})`.

Confirmed live — a well-formed, correctly authed rename request is rejected at schema
validation, before the op dispatcher is even reached:

```
send: {"proto":1,"op":"rename","short":"0b5dab67","title":"x","auth":"<control.key>"}
recv: {"ok":false,"error":"malformed request: Invalid input","code":"EUNKNOWN"}
```

The rendezvous socket handles only `shutdown` / `repaint` / `attacher-caps` / `reply` (§3), so
the repo's current frame would be ignored even if it were correctly authed — it is not a
`type` the rv server knows, and `subtype` is not the discriminator there.

`rename_session` **does exist in the product**, but only as an **SDK/bridge control_request**
over the stdio bridge protocol (`{type:"control_request",request_id,request:{subtype:"rename_session",title}}`),
not over any unix socket:

```js
case"rename_session":{if(typeof e.request.title!=="string") ...
v.object({subtype:v.literal("rename_session"),title:v.string()}).describe("Sets the ...")
```

That transport is only reachable by whoever owns the session's stdio — i.e. the process that
launched it. An external viewer cannot get to it.

**If it ever becomes achievable**, the frame would be a newline-terminated JSON line on
`control.sock`:

```json
{"proto":1,"op":"rename","short":"<8 hex>","auth":"<32-hex control.key>","title":"New name"}
```

but that op does not exist today and would require an upstream change. **Speculation** (labelled
as such): the roster is written only by the daemon (`rIe((T)=>{delete T.workers[A.short]})` is
the only mutation shape seen), so poking a name into `roster.json` directly would be a write to
a file agent-viewer's own invariants say to treat as read-only, and would be clobbered on the
next daemon write. Not recommended, not investigated further.

---

## 5. Version fragility

There **is** a real version handshake, and it is the daemon's, not a guess.

`proto` is a required integer field on every control-socket request, bounded
`v.number().int().min($dr).max(Um)`, and the daemon rejects out-of-range values *before*
parsing the op:

```
send: {"proto":99,"op":"list"}
recv: {"ok":false,"error":"proto mismatch (server=1, client=99) — background service and CLI
        versions differ; restart claude","code":"EPROTO","serverProto":1,"serverVersion":"2.1.220"}
```

So on 2.1.220, `Um` (current proto) `= 1`. The error reply carries **both `serverProto` and
`serverVersion`**, which is the ideal gate: agent-viewer can send `proto:1`, and on `EPROTO`
degrade gracefully to forking `claude attach` while showing the version skew in the footer.
`cliVersion` in the roster gives a cheaper pre-check.

Fragility assessment:

- **Control-socket attach (Path A): moderate risk, loud failure.** The proto field means an
  incompatible daemon says so explicitly rather than corrupting the stream. The request shape
  is zod-validated, so a renamed/added required field yields `"malformed request: ..."` —
  also loud. Optional fields (`attachId`, `caps`, `holdingFrame`) are additive-safe.
  The pinned risks are: `proto` bumping to 2 with a changed attach shape, and the `auth===void 0`
  legacy path being removed (already telemetered as a dead-probe — send `auth` and this is moot).
- **Direct pty framing (Path B): higher risk, silent failure.** The 5-byte header, kind
  constants, and ctrl verb names are internal implementation details with no version
  negotiation at all. The `hello` frame does carry `"version":"2.1.220"`, which is the only
  gate available, and it arrives *after* you have already committed to the framing.
- Both depend on roster field names (`ptySock`, `ptyAuth`, `rvAuth`, `decModes`) which the repo
  already reads, so that exposure is not new.

Claude Code ships multiple releases a week (three versions are installed on this box:
2.1.218/2.1.219/2.1.220). Expect the daemon internals to move. The mitigation that makes this
tolerable is not "pin a version" but **fallback**: attempt the socket attach, and on `EPROTO` /
`EUNKNOWN` / connect failure, fall back to forking `claude attach` — which the repo already has
working.

---

## 6. `claude rm` and worktree deletion

`claude rm <id>` help text, verbatim:

```
Usage: claude rm <id>

  Delete a background session and its worktree. Unlike `stop`, works on already-exited sessions.
```

The implementation (`deleteJob`, aliased `Z3e`, around offset `262895620`) is **heavily guarded**.
It only removes a worktree if **all** of these hold; otherwise it keeps the worktree, reports
`kept <id> — worktree ...`, and prints where it was left:

1. The kill is *confirmed* — otherwise `{removed:!1,errorCode:"kill_unconfirmed"}` and it skips
   both job dir and worktree ("skipping jobdir/worktree removal to avoid stranding a live worker").
2. The worktree is not claimed by another running job's `state.json` (`l="in_use"`).
3. It is not also recorded by another *settled* job's `state.json` (`l="shared_record"`), and
   sibling records are readable (`l="records_unreadable"`).
4. It is not locked by a live Claude Code process (`l="live_lock"`).
5. It is **not dirty**: `else if(p&&!t.force)l="dirty"` — "deleteJob: worktree has uncommitted
   changes, kept ...". `--force` overrides this one.
6. It has **no commits that are on no remote**: `l="unpushed"` — "has commits that are on no
   remote, kept".

If all pass it calls `removeAgentWorktree(path, worktreeBranch, ...)` — which deletes the
worktree directory **and the branch**.

Sessions with no `worktreePath` (`if(r?.worktreePath)` guard) simply have their job dir removed;
nothing git-related happens.

**Implication for agent-viewer's hotkey**: binding `claude rm` to a key is defensible — the CLI
will not silently eat uncommitted or unpushed work — but it *is* destructive for a clean,
pushed worktree session (dir + branch gone, no prompt). It warrants a confirmation step and a
footer that surfaces the `kept ... — worktree <reason>` output rather than treating a non-zero
"kept" result as success. Note `rm` exits **1** with `Usage:` on a bad/ambiguous prefix, and the
short id is matched by **prefix** (`.filter((p)=>p.startsWith(e))`), erroring on ambiguity.

---

## 7. Effort vs payoff

Bar stated by the user: *"real time is better, but not if it requires heroic engineering."*

**Recommendation: do it, via control-socket attach (Path A), with a `claude attach` fallback.**
This is not heroic engineering.

What it costs:
- One `UnixStream` connect, one JSON line write, one JSON line read, then a byte pump between
  the socket and the TUI's terminal surface. Roughly the same shape as the PTY attach code
  already in `-core`, minus the child-process supervision.
- A second short-lived connection for each resize, keyed by `attachId`.
- Reading `~/.claude/daemon/control.key` (one small file) and locating the instance's
  `control.sock` under `/tmp/cc-daemon-$UID/<instance>/` — the same glob-and-discover discipline
  the repo already applies to `state_*.sqlite`.
- An `EPROTO`/error branch that falls back to the existing fork path.

What it buys: no second 262 MB Node/Bun CLI process per attach, sub-second attach, and the
`decModes` + `state`/`tempo`/`stale` metadata handed back in the attach reply for free.

What it does **not** buy: rename. That stays a `NOT-PRACTICAL` no-op-with-footer-notice, which
is exactly what the repo's capability model already prescribes for unsupported actions. Stop
sending the current `rename_session` frame to `rendezvousSock` regardless — §3 shows it evicts
the daemon's supervisor connection every time it fires, which is a real (if transient) side
effect for zero benefit.

---

## 8. What I could not determine

- **Whether the `attach` reply's raw-byte stream needs the client to emit the `decModes`
  enable sequences itself.** The daemon writes `b.decModeSnapshot().map(d7).join("")` in one
  code path (the holding-frame flush) but the plain path appears to rely on the session's own
  output. Not tested with a real interactive round-trip.
- **Bracketed-paste / APC out-of-band messages.** The daemon emits `daemonDetachApc` /
  `parseDetachMsg` (`tIe` / `pks`) wrappers carrying `EKICKED:`, `ESTALLED:`, `ETRANSIENT`
  strings inline in the byte stream. I identified the mechanism but did not decode the exact
  APC wire format, so agent-viewer would need to either parse or strip these.
- **`attacher-caps` effects in practice.** `b.sendAttacherCaps(A.caps??null)` sets
  `process.env.BROWSER`, color level, and system theme inside the live session. I did not test
  what a wrong/absent `caps` object does to an already-attached human's session.
- **Whether the daemon's `list` output includes a session title field usable as the rename
  target.** The live `list` reply I captured did not obviously carry one; I did not enumerate
  the full record schema.
- **Multi-instance daemons.** Only one instance dir (`13512c9a`) existed on this box, so the
  instance-discovery loop (`if(!await nMy(vb.join(i,"control.sock")))continue;` over
  `cc-daemon-${uid}` with `/^[a-f0-9]{16}$/` names) was read but not exercised.
- **Windows.** All findings are Linux-path only; the bundle has substantial `Mt()==="windows"`
  divergence (named pipes instead of unix sockets, single-attacher kick semantics).
- I did **not** verify that `claude rm` actually deletes a worktree end-to-end. Both throwaway
  probe sessions were non-worktree (`cd /tmp`), so only the job-dir path was exercised
  (`removed 864d8566`, `removed 0b5dab67`). The worktree conditions above are read from the
  bundle, not observed.
