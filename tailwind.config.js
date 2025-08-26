/** @type {import('tailwindcss').Config} */
module.exports = {
  darkMode: 'class',
  content: [
    // include all rust files in the src directory
    "./src/**/*.{rs,html,css}",
    // include all rust files in the packages directory
    "./packages/**/*.{rs,html,css}",
    // include all rust files in the apps directory
    "./apps/**/*.{rs,html,css}",
  ],
  theme: {
    extend: {
      keyframes: {
        'pulse-fast': {
          '0%, 100%': { opacity: '1' },
          '50%': { opacity: '0.5' },
        },
        'pulse-medium': {
          '0%, 100%': { opacity: '1' },
          '50%': { opacity: '0.5' },
        },
        'pulse-slow': {
          '0%, 100%': { opacity: '1' },
          '50%': { opacity: '0.5' },
        },
      },
      animation: {
        'pulse-fast': 'pulse-fast 1.2s cubic-bezier(0.4, 0, 0.6, 1) infinite',
        'pulse-medium': 'pulse-medium 1.2s cubic-bezier(0.4, 0, 0.6, 1) infinite 0.15s',
        'pulse-slow': 'pulse-slow 1.2s cubic-bezier(0.4, 0, 0.6, 1) infinite 0.3s',
      },
    },
  },
  plugins: [
    require('@tailwindcss/typography'),
  ],
}