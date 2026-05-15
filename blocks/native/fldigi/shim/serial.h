/* Shim for <serial.h>. The real Cserial is rig CAT/PTT serial I/O
 * (termios), pulled in for FSK keying on the TX path. Headless RX-only
 * decode never opens a port — an all-inline no-op Cserial removes the
 * serial.cxx/estrings/re/FLTK dependency entirely. */
#ifndef FERRITE_FLDIGI_SHIM_SERIAL_H
#define FERRITE_FLDIGI_SHIM_SERIAL_H

#include <string>

class Cserial {
public:
	Cserial() {}
	~Cserial() {}
	bool OpenPort() { return false; }
	void ClosePort() {}
	bool IsOpen() { return false; }
	void Device(std::string) {}
	std::string Device() { return ""; }
	void Baud(int) {}
	int  Baud() { return 0; }
	void SetDTR(bool) {}
	void SetRTS(bool) {}
	void setRTS(bool) {}
	void setDTR(bool) {}
	bool ReadByte(char &) { return false; }
	int  ReadBuffer(unsigned char *, int) { return 0; }
	int  WriteBuffer(unsigned char *, int) { return 0; }
	void FlushBuffer() {}
	void DTR(bool) {}
	void DTRptt(bool) {}
	void RTS(bool) {}
	void RTSptt(bool) {}
	void RTSCTS(bool) {}
	void Stopbits(int) {}
	void RestoreTIO(bool = false) {}
};

#endif /* FERRITE_FLDIGI_SHIM_SERIAL_H */
