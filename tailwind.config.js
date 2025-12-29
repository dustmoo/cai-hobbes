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
      colors: {
        primary: {
          100: '#D6E0F0',
          200: '#ADC2E0',
          300: '#84A4D1',
          400: '#5B86C1',
          500: '#3B5998', // YinMn Blue
          600: '#2F477A',
          700: '#23355C',
          800: '#17233E',
          900: '#0B1120',
        },
        secondary: {
          100: '#F0D6E0',
          200: '#E0ADC2',
          300: '#D184A4',
          400: '#C15B86',
          500: '#C75681', // Magenta
          600: '#9F4567',
          700: '#77344D',
          800: '#4F2233',
          900: '#271119',
        },
        accent: {
          100: '#F0E0D6',
          200: '#E0C2AD',
          300: '#D1A484',
          400: '#C1865B',
          500: '#CC9423', // Harvest Gold
          600: '#A3761C',
          700: '#7A5815',
          800: '#513A0E',
          900: '#281C07',
        },
        // Semantic Backgrounds
        'light-bg': '#FFEE9D', // Flax
        'dark-bg': '#1A1A1A', // Eerie Black
        'dark-section': '#242424',
        'dark-card': '#2D2D2D',
        'dark-input': '#242424',
        'neutral-light': '#FFF8E1',

        // Semantic Text
        'light-text': '#1A1A1A',
        'dark-text': '#FFFFFF',
      },
      fontFamily: {
        'inter-logo': ['"Inter"', 'sans-serif'],
      },
      typography: ({ theme }) => ({
        DEFAULT: {
          css: {
            '--tw-prose-code': theme('colors.pink[400]'),
            'code': {
              color: '#c0c5ce', // base16-ocean.dark default text
              fontFamily: "'Fira Code', 'Courier New', monospace",
            },
            'pre': {
              backgroundColor: '#2b303b', // base16-ocean.dark background
              color: '#c0c5ce',
            },
            h1: { color: theme('colors.secondary.700') },
            h2: { color: theme('colors.primary.700') },
            h3: { color: theme('colors.primary.700') },
            h4: { color: theme('colors.primary.600') },
            p: { color: theme('colors.light-text') },
            a: {
              color: theme('colors.primary.500'),
              '&:hover': { textDecoration: 'underline' },
            },
            blockquote: { borderLeftColor: theme('colors.accent.500') },
          },
        },
        invert: {
          css: {
            h1: { color: theme('colors.secondary.300') },
            h2: { color: theme('colors.secondary.300') },
            h3: { color: theme('colors.primary.300') },
            h4: { color: theme('colors.primary.300') },
            p: { color: theme('colors.dark-text') },
            // Remove hr styling for cleaner bubble display
            hr: { 
              display: 'none',
            },
            // Adjust link spacing
            'a': {
              padding: '0',
              margin: '0',
            },
          },
        },
      }),
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