package com.termux.terminal;

/** The far end of a remote PTY: bytes typed by the user go out, size changes are announced, close ends it. */
public interface RemoteTerminalTransport {
    void send(byte[] data, int offset, int count);

    void resize(int columns, int rows);

    void close();
}
