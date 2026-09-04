package com.termux.terminal;

/**
 * A {@link TerminalSession} whose process lives on another machine. Output bytes arrive through
 * {@link #receive(byte[], int)}; input bytes leave through a {@link RemoteTerminalTransport}.
 * Lives in the com.termux.terminal package because {@link #mEmulator} is package private.
 */
public class RemoteTerminalSession extends TerminalSession {

    private final RemoteTerminalTransport mTransport;
    private final Integer mRemoteTranscriptRows;
    private volatile boolean mOpen = true;

    public RemoteTerminalSession(RemoteTerminalTransport transport, Integer transcriptRows, TerminalSessionClient client) {
        super("remote", "/", new String[0], new String[0], transcriptRows, client);
        mTransport = transport;
        mRemoteTranscriptRows = transcriptRows;
    }

    @Override
    public void initializeEmulator(int columns, int rows) {
        mEmulator = new TerminalEmulator(this, columns, rows, mRemoteTranscriptRows, mClient);
        mShellPid = 1;
        mTransport.resize(columns, rows);
    }

    @Override
    public void updateSize(int columns, int rows) {
        if (mEmulator == null) {
            initializeEmulator(columns, rows);
        } else {
            mEmulator.resize(columns, rows);
            mTransport.resize(columns, rows);
        }
    }

    @Override
    public void write(byte[] data, int offset, int count) {
        if (mOpen) mTransport.send(data, offset, count);
    }

    /** Feed bytes that arrived from the remote PTY. Call on the main thread. */
    public void receive(byte[] data, int length) {
        if (mEmulator == null) return;
        mEmulator.append(data, length);
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
