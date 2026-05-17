// Default whisper `initial_prompt` — a dense amateur-radio vocabulary
// seed. Whisper biases its decode toward words present in the prompt,
// so packing it with ham lingo dramatically improves recognition of
// callsigns, Q-codes, signal reports and on-air phrasing over a
// generic English model — the single highest-leverage knob for this
// use case.
//
// Surfaced + editable in the Transcript tab (persisted as
// `client.transcribe.prompt`; empty there means "use this default").
// The worker appends recently-heard callsigns after this base so the
// bias self-reinforces within a QSO.
//
// Note: whisper.cpp truncates the prompt to roughly the last ~224
// tokens of context, so the *tail* matters most — keep the highest-
// value, most-current vocabulary toward the end (the rolling callsigns
// the worker appends are naturally last, which is what we want).

export const DEFAULT_HAM_PROMPT = `Amateur radio HF voice contact transcript. Phonetic alphabet: Alpha Bravo Charlie Delta Echo Foxtrot Golf Hotel India Juliett Kilo Lima Mike November Oscar Papa Quebec Romeo Sierra Tango Uniform Victor Whiskey X-ray Yankee Zulu, with numbers zero one two three four five six seven eight nine niner.

Common Q-codes: QRZ who is calling, QTH location, QSL confirm, QSO contact, QSY change frequency, QRM interference, QRN static, QRP low power, QRO high power, QSB fading, QRT closing down, QRV ready, QRX stand by, QRL busy.

Operating phrases: CQ CQ CQ calling CQ contest, this is, over to you, back to you, go ahead, roger roger, copy that, solid copy, you are five nine, five nine plus, RST report readability strength tone, fifty nine, good signal, weak signal, in and out of the noise, working you, calling you, standing by, listening, clear, seventy three, eighty eight, best regards, thanks for the contact, nice to meet you, first time, log it, QSL via the bureau, QSL via LoTW, eQSL, direct with SASE.

Exchange and DX: signal report and serial number, grid square locator, ITU zone, CQ zone, the pileup, split operation, listening up, working DX, DXpedition, IOTA island, POTA parks on the air, SOTA summits, contest exchange, sweepstakes, field day, the run frequency, search and pounce, dupe, in the log.

Bands and modes: eighty meters, forty meters, twenty meters, seventeen meters, fifteen meters, twelve meters, ten meters, six meters, two meters, seventy centimeters, SSB upper sideband lower sideband, USB LSB, AM, FM, CW Morse code, the WARC bands, the General band, the Extra portion.

Equipment and conditions: the rig, transceiver, the amplifier, the linear, beam antenna, dipole, vertical, the tower, the rotator, S-meter, the noise floor, band conditions, propagation, the grey line, sporadic E, the solar flux, K index, antenna tuner, the feedline.

Stations heard on frequency: `;
