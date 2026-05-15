/* Shim replacement for fldigi's <sound.h> — the real one pulls
 * libsndfile / portaudio / libsamplerate. Headless decode feeds audio
 * in through the C ABI; fldigi's SoundBase is only touched on the TX
 * path (TXscard->Write*, guarded, never reached RX-only). A minimal
 * non-abstract stub suffices. */
#ifndef FERRITE_FLDIGI_SHIM_SOUND_H
#define FERRITE_FLDIGI_SHIM_SOUND_H

#include <string>
#include <cstring>
#include <cstddef>

/* modem.cxx's TX writes throw/catch SndException. Mirror the real
 * (sound.h) interface so the try/catch compiles unchanged. */
class SndException {
public:
	SndException(int err_ = 0)
		: err(err_), msg(std::string("Sound error: ") + strerror(err_)) {}
	SndException(const char *msg_) : err(1), msg(msg_) {}
	SndException(int err_, const std::string &msg_) : err(err_), msg(msg_) {}
	virtual ~SndException() throw() {}
	const char *what() const throw() { return msg.c_str(); }
	int error() const { return err; }
protected:
	int err;
	std::string msg;
};

class SoundBase {
public:
	SoundBase() {}
	virtual ~SoundBase() {}
	virtual size_t Write(double *, size_t) { return 0; }
	virtual size_t Write_stereo(double *, double *, size_t) { return 0; }
	virtual size_t Read(float *, size_t) { return 0; }
	virtual int    Audio(std::string) { return 0; }
	virtual int    Open(int, int = 8000) { return 0; }
	virtual void   Close(unsigned = 0) {}
	virtual void   flush(unsigned = 0) {}
	int  Frequency() { return sample_frequency; }
	void Frequency(int f) { sample_frequency = f; }
protected:
	int sample_frequency = 8000;
};

extern SoundBase *RXscard;
extern SoundBase *TXscard;

#endif /* FERRITE_FLDIGI_SHIM_SOUND_H */
