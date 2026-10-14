// Speaker / talkgroup colours for transcripts and call logs.
//
// Ten categorical colours, colour-blind-safe on the dark theme (Okabe–
// Ito's eight, minus black, plus three distinct extras), indexed by the
// server-assigned `speaker.colour` so every tab and view agrees. Colour
// is never the only cue — every coloured element also carries its label.

export const PALETTE: ReadonlyArray<string> = [
  '#56B4E9', // sky blue
  '#E69F00', // orange
  '#009E73', // bluish green
  '#F0E442', // yellow
  '#CC79A7', // reddish purple
  '#D55E00', // vermillion
  '#7FD3C3', // pale teal
  '#B9A3E3', // lavender
  '#9DC25B', // olive green
  '#F4A6A6', // rose
];

export const NEUTRAL = '#94a3b8'; // slate-400: unknown speaker

export function speakerColour(index: number | undefined): string {
  return index === undefined
    ? NEUTRAL
    : PALETTE[((index % PALETTE.length) + PALETTE.length) % PALETTE.length];
}

// Talkgroups have no server-assigned colour: assign in first-seen order,
// shared by every view in this tab so both panes agree.
const groupOrder = new Map<string, number>();
export function groupColour(group: string | undefined): string {
  if (!group) return NEUTRAL;
  let i = groupOrder.get(group);
  if (i === undefined) {
    i = groupOrder.size;
    groupOrder.set(group, i);
  }
  return speakerColour(i);
}
