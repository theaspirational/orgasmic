/// <reference types="vite/client" />

// Fontsource packages ship only CSS (their root export resolves to a .css file),
// so the bare side-effect import has no type. vite/client's `*.css` declaration
// keys on the specifier string, which doesn't end in `.css` here — declare it.
declare module '@fontsource-variable/geist';
