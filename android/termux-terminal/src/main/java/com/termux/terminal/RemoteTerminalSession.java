package com.termux.terminal;

import java.io.ByteArrayOutputStream;

/**
 * A {@link TerminalSession} whose process lives on another machine. Output bytes arrive through
 * {@link #receive(byte[], int)}; input bytes leave through a {@link RemoteTerminalTransport}.
 * Lives in the com.termux.terminal package because {@link #mEmulator} is package private.
 */
public class RemoteTerminalSession extends TerminalSession {

    /**
     * Cap on bytes held while the view is still unmeasured. The window is normally one layout
     * pass; a view that never lays out is not on screen, so past the cap the tail is dropped
     * rather than grown without bound. The server repaints the whole screen on the next
     * connect, so a drop here costs nothing a redraw does not restore.
     */
    static final int PENDING_LIMIT_BYTES = 1 << 21;

    private final RemoteTerminalTransport mTransport;
    private final Integer mRemoteTranscriptRows;
    private volatile boolean mOpen = true;

    /**
     * Output that arrived before the emulator existed. The socket opens as soon as the console
     * composes, which is before the view has been measured, so the server's hello and its
     * one-shot redraw both land in this window; without the buffer the screen stays blank until
     * the CLI happens to repaint.
     */
    private ByteArrayOutputStream mPending = new ByteArrayOutputStream();

    public RemoteTerminalSession(RemoteTerminalTransport transport, Integer transcriptRows, TerminalSessionClient client) {
        super("remote", "/", new String[0], new String[0], transcriptRows, client);
        mTransport = transport;
        mRemoteTranscriptRows = transcriptRows;
    }

    @Override
    public void initializeEmulator(int columns, int rows, int cellWidthPixels, int cellHeightPixels) {
        mEmulator = new TerminalEmulator(this, columns, rows, cellWidthPixels, cellHeightPixels, mRemoteTranscriptRows, mClient);
        mShellPid = 1;
        flushPending();
        mTransport.resize(columns, rows);
    }

    @Override
    public void updateSize(int columns, int rows, int cellWidthPixels, int cellHeightPixels) {
        if (mEmulator == null) {
            initializeEmulator(columns, rows, cellWidthPixels, cellHeightPixels);
        } else {
            mEmulator.resize(columns, rows, cellWidthPixels, cellHeightPixels);
            mTransport.resize(columns, rows);
        }
    }

    @Override
    public void write(byte[] data, int offset, int count) {
        if (mOpen) mTransport.send(data, offset, count);
    }

    /** Feed bytes that arrived from the remote PTY. Call on the main thread. */
    public void receive(byte[] data, int length) {
        if (mEmulator == null) {
            int room = PENDING_LIMIT_BYTES - mPending.size();
            if (room > 0) mPending.write(data, 0, Math.min(length, room));
            return;
        }
        mEmulator.append(data, length);
        notifyScreenUpdate();
    }

    /**
     * Replay everything buffered before the emulator existed, in arrival order, as the first
     * bytes the fresh screen sees. Only ever runs from {@link #initializeEmulator}, which the
     * view reaches once per session, so the later resize-driven calls into
     * {@link #updateSize(int, int, int, int)} cannot replay it a second time.
     */
    private void flushPending() {
        byte[] pending = mPending.toByteArray();
        mPending = new ByteArrayOutputStream();
        if (pending.length == 0) return;
        mEmulator.append(pending, pending.length);
        notifyScreenUpdate();
    }

    /** The remote side reported the process exited. */
    public void remoteExited(int exitStatus) {
        mOpen = false;
        cleanupResources(exitStatus);
        mClient.onSessionFinished(this);
    }

    @Override
    public void finishIfRunning() {
        if (mOpen) {
            mOpen = false;
            mTransport.close();
        }
    }

    @Override
    public synchronized boolean isRunning() {
        return mOpen;
    }
}
