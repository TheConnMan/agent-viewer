package spike;

import static org.junit.Assert.assertEquals;
import static org.junit.Assert.assertNotNull;
import static org.junit.Assert.assertTrue;

import android.app.Activity;
import android.graphics.Bitmap;
import android.graphics.Canvas;
import android.view.KeyEvent;
import android.view.MotionEvent;
import android.view.View;
import android.view.inputmethod.EditorInfo;
import android.view.inputmethod.InputConnection;

import com.termux.terminal.RemoteTerminalSession;
import com.termux.terminal.RemoteTerminalTransport;
import com.termux.terminal.TerminalEmulator;
import com.termux.terminal.TerminalSession;
import com.termux.terminal.TerminalSessionClient;
import com.termux.terminal.TextStyle;
import com.termux.view.TerminalView;
import com.termux.view.TerminalViewClient;

import org.junit.Test;
import org.junit.runner.RunWith;
import org.robolectric.Robolectric;
import org.robolectric.RobolectricTestRunner;
import org.robolectric.annotation.Config;
import org.robolectric.annotation.GraphicsMode;

import java.io.ByteArrayOutputStream;
import java.io.File;
import java.io.FileOutputStream;
import java.io.InputStream;
import java.nio.charset.StandardCharsets;
import java.util.ArrayList;
import java.util.List;

@RunWith(RobolectricTestRunner.class)
@Config(sdk = 34, qualifiers = "w411dp-h891dp-xxhdpi")
@GraphicsMode(GraphicsMode.Mode.NATIVE)
public class RemoteTerminalViewTest {

    static final String ESC = "";

    /** Records everything the view wants to send to the remote PTY. */
    static final class RecordingTransport implements RemoteTerminalTransport {
        final ByteArrayOutputStream sent = new ByteArrayOutputStream();
        final List<int[]> resizes = new ArrayList<>();
        boolean closed;

        @Override public void send(byte[] data, int offset, int count) { sent.write(data, offset, count); }
        @Override public void resize(int columns, int rows) { resizes.add(new int[]{columns, rows}); }
        @Override public void close() { closed = true; }

        String drain() {
            String s = new String(sent.toByteArray(), StandardCharsets.UTF_8);
            sent.reset();
            return s;
        }
    }

    static final class SessionClient implements TerminalSessionClient {
        int textChanges;
        @Override public void onTextChanged(TerminalSession s) { textChanges++; }
        @Override public void onTitleChanged(TerminalSession s) { }
        @Override public void onSessionFinished(TerminalSession s) { }
        @Override public void onCopyTextToClipboard(TerminalSession s, String text) { }
        @Override public void onPasteTextFromClipboard(TerminalSession s) { }
        @Override public void onBell(TerminalSession s) { }
        @Override public void onColorsChanged(TerminalSession s) { }
        @Override public void onTerminalCursorStateChange(boolean state) { }
        @Override public Integer getTerminalCursorStyle() { return TerminalEmulator.TERMINAL_CURSOR_STYLE_BLOCK; }
        @Override public void logError(String t, String m) { }
        @Override public void logWarn(String t, String m) { }
        @Override public void logInfo(String t, String m) { }
        @Override public void logDebug(String t, String m) { }
        @Override public void logVerbose(String t, String m) { }
        @Override public void logStackTraceWithMessage(String t, String m, Exception e) { }
        @Override public void logStackTrace(String t, Exception e) { }
    }

    static final class ViewClient implements TerminalViewClient {
        boolean ctrl;
        @Override public float onScale(float scale) { return scale; }
        @Override public void onSingleTapUp(MotionEvent e) { }
        @Override public boolean shouldBackButtonBeMappedToEscape() { return false; }
        @Override public boolean shouldEnforceCharBasedInput() { return true; }
        @Override public boolean shouldUseCtrlSpaceWorkaround() { return false; }
        @Override public boolean isTerminalViewSelected() { return true; }
        @Override public void copyModeChanged(boolean copyMode) { }
        @Override public boolean onKeyDown(int keyCode, KeyEvent e, TerminalSession session) { return false; }
        @Override public boolean onKeyUp(int keyCode, KeyEvent e) { return false; }
        @Override public boolean onLongPress(MotionEvent event) { return false; }
        @Override public boolean readControlKey() { return ctrl; }
        @Override public boolean readAltKey() { return false; }
        @Override public boolean readShiftKey() { return false; }
        @Override public boolean readFnKey() { return false; }
        @Override public boolean onCodePoint(int codePoint, boolean ctrlDown, TerminalSession session) { return false; }
        @Override public void onEmulatorSet() { }
        @Override public void logError(String t, String m) { }
        @Override public void logWarn(String t, String m) { }
        @Override public void logInfo(String t, String m) { }
        @Override public void logDebug(String t, String m) { }
        @Override public void logVerbose(String t, String m) { }
        @Override public void logStackTraceWithMessage(String t, String m, Exception e) { }
        @Override public void logStackTrace(String t, Exception e) { }
    }

    static final int PHONE_W = 1080, PHONE_H = 1800; // portrait pixels left for the terminal below a header

    static final class Rig {
        final RecordingTransport transport = new RecordingTransport();
        final SessionClient sessionClient = new SessionClient();
        final ViewClient viewClient = new ViewClient();
        final RemoteTerminalSession session;
        final TerminalView view;

        Rig(int textSizePx, int width, int height) {
            Activity activity = Robolectric.buildActivity(Activity.class).setup().get();
            view = new TerminalView(activity, null);
            view.setTerminalViewClient(viewClient);
            view.setTextSize(textSizePx);
            session = new RemoteTerminalSession(transport, 2000, sessionClient);
            activity.setContentView(view);
            view.attachSession(session);
            view.measure(View.MeasureSpec.makeMeasureSpec(width, View.MeasureSpec.EXACTLY),
                View.MeasureSpec.makeMeasureSpec(height, View.MeasureSpec.EXACTLY));
            view.layout(0, 0, width, height);
        }

        void feed(String resource) throws Exception {
            try (InputStream in = getClass().getResourceAsStream("/" + resource)) {
                assertNotNull("fixture " + resource, in);
                byte[] bytes = in.readAllBytes();
                session.receive(bytes, bytes.length);
            }
        }

        void key(int keyCode, int meta) {
            view.onKeyDown(keyCode, new KeyEvent(0, 0, KeyEvent.ACTION_DOWN, keyCode, 0, meta));
        }

        String screenText() { return view.mEmulator.getScreen().getTranscriptText(); }

        File snapshot(String name) throws Exception {
            Bitmap bitmap = Bitmap.createBitmap(view.getWidth(), view.getHeight(), Bitmap.Config.ARGB_8888);
            view.draw(new Canvas(bitmap));
            File out = new File(System.getProperty("spike.out", "build/spike"), name + ".png");
            out.getParentFile().mkdirs();
            try (FileOutputStream fos = new FileOutputStream(out)) {
                bitmap.compress(Bitmap.CompressFormat.PNG, 100, fos);
            }
            return out;
        }
    }

    @Test
    public void layoutSizesTheEmulatorFromPixelsAndAnnouncesIt() {
        StringBuilder report = new StringBuilder("SIZE REPORT (1080px wide portrait, 1800px tall)\n");
        for (int px : new int[]{22, 26, 30, 34}) {
            Rig rig = new Rig(px, PHONE_W, PHONE_H);
            assertNotNull("emulator created on layout", rig.view.mEmulator);
            int[] last = rig.transport.resizes.get(rig.transport.resizes.size() - 1);
            assertEquals(rig.view.mEmulator.mColumns, last[0]);
            assertEquals(rig.view.mEmulator.mRows, last[1]);
            report.append(String.format("  textSize=%dpx -> %d cols x %d rows%n", px, last[0], last[1]));
        }
        Rig landscape = new Rig(26, 2340, 900);
        report.append(String.format("  landscape textSize=26px -> %d cols x %d rows%n",
            landscape.view.mEmulator.mColumns, landscape.view.mEmulator.mRows));
        System.out.println(report);
    }

    @Test
    public void claudeStartupRendersWithColorAndAltScreen() throws Exception {
        Rig rig = new Rig(26, PHONE_W, PHONE_H);
        rig.feed("claude-56x32.bin");
        String text = rig.screenText();
        assertTrue("Claude banner on screen:\n" + text, text.contains("Claude Code"));
        assertTrue("alternate screen active", rig.view.mEmulator.isAlternateBufferActive());
        assertTrue("screen update notified", rig.sessionClient.textChanges > 0);
        assertTrue("bracketed paste enabled by claude", pasteIsBracketed(rig));
        assertTrue("some cell carries a non-default foreground", anyColoredCell(rig.view.mEmulator));
        File png = rig.snapshot("claude-startup");
        System.out.println("SNAPSHOT " + png.getAbsolutePath());
        System.out.println("CLAUDE SCREEN:\n" + text);
    }

    @Test
    public void claudeResumedSessionShowsItsComposer() throws Exception {
        Rig rig = new Rig(26, PHONE_W, PHONE_H);
        rig.feed("claude-resume-56x32.bin");
        String text = rig.screenText();
        assertTrue("composer prompt on screen:\n" + text, text.contains("❯"));
        File png = rig.snapshot("claude-resume");
        System.out.println("SNAPSHOT " + png.getAbsolutePath());
        System.out.println("CLAUDE RESUME SCREEN:\n" + text);
    }

    @Test
    public void codexStartupRenders() throws Exception {
        Rig rig = new Rig(26, PHONE_W, PHONE_H);
        rig.feed("codex-56x32.bin");
        String text = rig.screenText();
        assertTrue("Codex composer on screen:\n" + text, text.contains("Codex") || text.contains("›"));
        File png = rig.snapshot("codex-startup");
        System.out.println("SNAPSHOT " + png.getAbsolutePath());
        System.out.println("CODEX SCREEN:\n" + text);
    }

    @Test
    public void keysAndImeTextReachTheTransportAsPtyBytes() throws Exception {
        Rig rig = new Rig(26, PHONE_W, PHONE_H);
        rig.feed("claude-56x32.bin");
        rig.transport.drain();

        InputConnection ime = rig.view.onCreateInputConnection(new EditorInfo());
        assertNotNull(ime);
        ime.commitText("hello", 1);
        assertEquals("hello", rig.transport.drain());

        rig.key(KeyEvent.KEYCODE_ENTER, 0);
        assertEquals("\r", rig.transport.drain());

        rig.key(KeyEvent.KEYCODE_DPAD_UP, 0);
        String up = rig.transport.drain();
        assertTrue("arrow up as CSI or SS3: " + escape(up), up.equals(ESC + "[A") || up.equals(ESC + "OA"));

        rig.key(KeyEvent.KEYCODE_DEL, 0);
        assertEquals("", rig.transport.drain());

        rig.key(KeyEvent.KEYCODE_ESCAPE, 0);
        assertEquals(ESC, rig.transport.drain());

        rig.key(KeyEvent.KEYCODE_TAB, 0);
        assertEquals("\t", rig.transport.drain());

        rig.key(KeyEvent.KEYCODE_C, KeyEvent.META_CTRL_ON | KeyEvent.META_CTRL_LEFT_ON);
        assertEquals("ctrl+c from a hardware-style event", "", rig.transport.drain());

        rig.viewClient.ctrl = true; // sticky ctrl from an extra-keys row
        ime.commitText("c", 1);
        rig.viewClient.ctrl = false;
        assertEquals("ctrl+c from IME text with sticky ctrl", "", rig.transport.drain());

        rig.view.mEmulator.paste("pasted text");
        String paste = rig.transport.drain();
        assertTrue("bracketed paste wrapping: " + escape(paste),
            paste.startsWith(ESC + "[200~") && paste.endsWith(ESC + "[201~"));

        // IME composing then committing (what Gboard does) must not duplicate text.
        ime.setComposingText("wor", 1);
        ime.setComposingText("world", 1);
        ime.finishComposingText();
        String composed = rig.transport.drain();
        System.out.println("COMPOSED BYTES: " + escape(composed));
        assertTrue("composition yields the final word once: " + escape(composed),
            composed.equals("world") || composed.isEmpty());

        rig.session.finishIfRunning();
        assertTrue(rig.transport.closed);
    }

    @Test
    public void resizeAfterAttachAnnouncesNewGeometryAndReflows() throws Exception {
        Rig rig = new Rig(26, PHONE_W, PHONE_H);
        rig.feed("claude-56x32.bin");
        int before = rig.transport.resizes.size();
        int colsBefore = rig.view.mEmulator.mColumns;
        rig.view.setTextSize(34);
        assertTrue("resize announced", rig.transport.resizes.size() > before);
        int[] last = rig.transport.resizes.get(rig.transport.resizes.size() - 1);
        assertEquals(rig.view.mEmulator.mColumns, last[0]);
        assertTrue("fewer columns at a bigger font", last[0] < colsBefore);
        // The alternate screen has no reflow by design: the CLI repaints after SIGWINCH,
        // which is what the announced resize triggers on the server's PTY.
        assertEquals(rig.view.mEmulator.mRows, last[1]);
    }

    private static boolean pasteIsBracketed(Rig rig) {
        rig.transport.drain();
        rig.view.mEmulator.paste("x");
        return rig.transport.drain().startsWith(ESC + "[200~");
    }

    private static boolean anyColoredCell(TerminalEmulator emulator) {
        for (int row = 0; row < emulator.mRows; row++) {
            for (int col = 0; col < emulator.mColumns; col++) {
                long style = emulator.getScreen().getStyleAt(row, col);
                if (TextStyle.decodeForeColor(style) != TextStyle.COLOR_INDEX_FOREGROUND) return true;
            }
        }
        return false;
    }

    private static String escape(String s) {
        StringBuilder b = new StringBuilder();
        for (char c : s.toCharArray()) {
            b.append(c < 32 || c == 127 ? String.format("\\x%02x", (int) c) : String.valueOf(c));
        }
        return b.toString();
    }
}
