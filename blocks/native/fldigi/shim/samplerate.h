// Minimal libsamplerate-compatible shim.
//
// The only vendored fldigi source that needs libsamplerate is
// rsid.cxx (it resamples the RX audio to RSID's internal 11.025 kHz
// before the Reed-Solomon FFT search). Rather than add a native
// libsamplerate dependency to the fldigi cc build, the shim provides
// the small slice of the SRC API rsid.cxx actually uses — same
// pattern as the shim's replacement fl_digi.h / configuration.h /
// qrunner.h. Backed by a stateful linear resampler
// (samplerate_shim.cxx): RSID is a robust Reed-Solomon-coded MFSK
// burst, so simple interpolation for the 8 k -> 11.025 k step is
// ample for detection. rsid.cxx stays byte-verbatim vendored.

#ifndef FERRITE_FLDIGI_SHIM_SAMPLERATE_H
#define FERRITE_FLDIGI_SHIM_SAMPLERATE_H

#ifdef __cplusplus
extern "C" {
#endif

typedef struct SRC_STATE_tag SRC_STATE;

typedef struct {
	float  *data_in;
	float  *data_out;
	long    input_frames;
	long    output_frames;
	long    input_frames_used;
	long    output_frames_gen;
	int     end_of_input;
	double  src_ratio;
} SRC_DATA;

// `converter_type` is libsamplerate's quality selector; ignored here
// (one fixed linear resampler). `channels` is always 1 for RSID.
SRC_STATE  *src_new(int converter_type, int channels, int *error);
SRC_STATE  *src_delete(SRC_STATE *state);
int         src_reset(SRC_STATE *state);
int         src_set_ratio(SRC_STATE *state, double new_ratio);
int         src_process(SRC_STATE *state, SRC_DATA *data);
const char *src_strerror(int error);

#ifdef __cplusplus
}
#endif

#endif // FERRITE_FLDIGI_SHIM_SAMPLERATE_H
