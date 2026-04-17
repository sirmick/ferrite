// adapter-static requires prerender for every route; this repo is a
// single-page app so we also disable SSR and let the browser take over.
export const prerender = true;
export const ssr = false;
