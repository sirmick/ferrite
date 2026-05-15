/* Shim replacement for fldigi's <plot_xy.h> (FLTK Fl_Widget).
 * modem.h holds `PLOT_XY *unk_pipe/ref_pipe` (fmt/analysis modes only,
 * unused on the RX text path). Need the struct + a no-op class. */
#ifndef FERRITE_FLDIGI_SHIM_PLOT_XY_H
#define FERRITE_FLDIGI_SHIM_PLOT_XY_H

#include <string>

struct PLOT_XY { double x; double y; };

class plot_xy {
public:
	plot_xy(int = 0, int = 0, int = 0, int = 0, const char * = 0) {}
	~plot_xy() {}
	void data_1(PLOT_XY *, int) {}
	void data_2(PLOT_XY *, int) {}
	void x_scale(double, double, double) {}
	void y_scale(double, double, double) {}
	void legends(bool = true) {}
	void set_x_legend(std::string) {}
	void set_y_legend(std::string) {}
	void reverse_x(bool) {}
	void show_1(bool) {}
	void show_2(bool) {}
	void redraw() {}
};

#endif /* FERRITE_FLDIGI_SHIM_PLOT_XY_H */
