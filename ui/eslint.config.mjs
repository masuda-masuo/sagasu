// ESLint flat config for the sagasu browse frontend (ui/).
//
// The page itself has no build step, no package manager and no npm
// dependency; this file only lets a global eslint parse-check the scripts in
// this directory (ui/app.js is the only JavaScript in the repo). No stylistic
// rules are imposed — the screen is one file and its style lives in style.css.
export default [
  {
    files: ["*.js"],
    languageOptions: {
      ecmaVersion: 2022,
      sourceType: "script",
    },
  },
  {
    files: ["tests/**/*.mjs"],
    languageOptions: {
      ecmaVersion: 2022,
      sourceType: "module",
    },
  },
];
