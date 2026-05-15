/* Shim for <test_signal.h>. modem.cxx probes
 * `test_signal_window && test_signal_window->visible()` to inject test
 * noise — always absent in headless decode (pointer stays null). */
#ifndef FERRITE_FLDIGI_SHIM_TEST_SIGNAL_H
#define FERRITE_FLDIGI_SHIM_TEST_SIGNAL_H

class test_signal_dialog {
public:
	int visible() { return 0; }
};
extern test_signal_dialog *test_signal_window;

#endif /* FERRITE_FLDIGI_SHIM_TEST_SIGNAL_H */
