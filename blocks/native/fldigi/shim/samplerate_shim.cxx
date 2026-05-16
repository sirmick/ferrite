// Stateful linear resampler implementing the slice of libsamplerate
// rsid.cxx uses. See samplerate.h for why this exists.
//
// Streaming contract (matches libsamplerate's src_process):
//  * resample `input_frames` of `data_in` into up to `output_frames`
//    of `data_out` at `state->ratio` (out_rate / in_rate);
//  * report `input_frames_used` / `output_frames_gen`;
//  * carry the fractional phase + last input sample across calls so a
//    chunked stream resamples seamlessly.
//
// Linear interpolation is intentionally simple: RSID symbols are
// Reed-Solomon-coded MFSK; the detector tolerates the mild HF rolloff
// far better than it would a missing dependency. One channel only.

#include <new>

#include "samplerate.h"

struct SRC_STATE_tag {
	double ratio;     // out_rate / in_rate
	double frac;      // phase within [prev, cur), in input samples
	float  prev;      // most recent input sample
	bool   have_prev; // false until the first input sample is taken
};

extern "C" {

SRC_STATE *src_new(int /*converter_type*/, int /*channels*/, int *error) {
	SRC_STATE *s = new (std::nothrow) SRC_STATE_tag();
	if (!s) {
		if (error) *error = 1;
		return 0;
	}
	s->ratio = 0.0;
	s->frac = 0.0;
	s->prev = 0.0f;
	s->have_prev = false;
	if (error) *error = 0;
	return s;
}

SRC_STATE *src_delete(SRC_STATE *state) {
	delete state;
	return 0;
}

int src_reset(SRC_STATE *state) {
	if (!state) return 1;
	state->frac = 0.0;
	state->prev = 0.0f;
	state->have_prev = false;
	return 0;
}

int src_set_ratio(SRC_STATE *state, double new_ratio) {
	if (!state || !(new_ratio > 0.0)) return 1;
	state->ratio = new_ratio;
	return 0;
}

int src_process(SRC_STATE *state, SRC_DATA *data) {
	if (!state || !data) return 1;
	double ratio = state->ratio > 0.0 ? state->ratio : data->src_ratio;
	if (!(ratio > 0.0)) return 1;

	const float *in = data->data_in;
	float *out = data->data_out;
	const long in_n = data->input_frames;
	const long out_n = data->output_frames;
	// Input-sample advance per output sample.
	const double step = 1.0 / ratio;

	long consumed = 0;
	long produced = 0;

	while (produced < out_n) {
		if (!state->have_prev) {
			if (consumed >= in_n) break;
			state->prev = in[consumed++];
			state->have_prev = true;
		}
		if (consumed >= in_n) break;
		float cur = in[consumed];
		while (state->frac < 1.0 && produced < out_n) {
			out[produced++] =
				state->prev + (float)state->frac * (cur - state->prev);
			state->frac += step;
		}
		if (state->frac >= 1.0) {
			state->frac -= 1.0;
			state->prev = cur;
			consumed++;
		}
	}

	data->input_frames_used = consumed;
	data->output_frames_gen = produced;
	return 0;
}

const char *src_strerror(int error) {
	return error ? "ferrite samplerate shim error" : "no error";
}

} // extern "C"
