/*
 * ferrite <-> mbelib shim: one opaque decoder per voice stream (each DMR
 * timeslot keeps its own previous-frame parameters), and a single
 * "decode one 72-bit AMBE+2 frame" call. The frame arrives already
 * deinterleaved into mbelib's ambe_fr[4][24] layout (one byte per bit)
 * by the Rust side; mbelib does the Golay/Hamming ECC, demodulation and
 * synthesis (mbe_processAmbe3600x2450Framef, the sequence dsd-fme uses).
 *
 * Same narrow ABI natively (static link) and in the browser (a sibling
 * Emscripten module, js/mbe_bridge.js).
 */
#include <stdlib.h>
#include <string.h>
#include "mbelib.h"

typedef struct {
	mbe_parms cur;
	mbe_parms prev;
	mbe_parms prev_enh;
} ferrite_mbe;

ferrite_mbe *ferrite_mbe_create(void)
{
	ferrite_mbe *h = (ferrite_mbe *)calloc(1, sizeof(ferrite_mbe));
	if (h) mbe_initMbeParms(&h->cur, &h->prev, &h->prev_enh);
	return h;
}

/* fr96: 4x24 bits, row-major, one byte per bit. out160: 20 ms at 8 kHz,
 * mbelib float scale (int16 / 7). Returns the corrected-error count
 * (errs2); < 0 on bad arguments. */
int ferrite_mbe_decode_ambe2450(ferrite_mbe *h, const unsigned char *fr96, float *out160)
{
	if (!h || !fr96 || !out160) return -1;
	char ambe_fr[4][24];
	char ambe_d[49];
	char err_str[64];
	int errs = 0, errs2 = 0;
	for (int r = 0; r < 4; r++)
		for (int c = 0; c < 24; c++)
			ambe_fr[r][c] = (char)(fr96[r * 24 + c] & 1);
	memset(ambe_d, 0, sizeof ambe_d);
	err_str[0] = 0;
	mbe_processAmbe3600x2450Framef(out160, &errs, &errs2, err_str, ambe_fr, ambe_d,
	                               &h->cur, &h->prev, &h->prev_enh, 3);
	return errs2;
}

void ferrite_mbe_destroy(ferrite_mbe *h)
{
	free(h);
}
