/* Shim for <audio_alert.h>. modem.cxx calls audio_alert->monitor(...)
 * on the TX-monitor path (guarded by progdefaults flags, default off).
 * Provide a no-op object so it compiles and links. */
#ifndef FERRITE_FLDIGI_SHIM_AUDIO_ALERT_H
#define FERRITE_FLDIGI_SHIM_AUDIO_ALERT_H

class cAudioAlerts {
public:
	void monitor(double * = 0, int = 0, int = 0, double = 0.0) {}
	void alert(std::string = "") {}
	void bark() {} void checkout() {} void doesnot() {}
};
extern cAudioAlerts *audio_alert;

#endif /* FERRITE_FLDIGI_SHIM_AUDIO_ALERT_H */
