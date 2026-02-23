import { createFileRoute } from '@tanstack/react-router'

export const Route = createFileRoute('/')({ component: LandingPage })

/* ─── data ───────────────────────────────────────────────────────── */

const features = [
  {
    title: 'Instant Transcription',
    desc: 'Fast transcription powered by OpenAI. Results appear as soon as you stop talking.',
  },
  {
    title: 'Global Hotkeys',
    desc: 'Hold to talk or toggle recording with a customizable shortcut, including Fn and Right Alt.',
  },
  {
    title: 'Auto-Insert',
    desc: 'Text appears right where your cursor is - editors, browsers, Slack, everywhere.',
  },
  {
    title: 'Flexible Auth',
    desc: 'Sign in with your ChatGPT account or use an OpenAI API key. Free with a ChatGPT subscription - much cheaper than WhisperFlow.',
  },
  {
    title: 'Usage Stats',
    desc: 'Track words dictated, words-per-minute, and daily streaks in a built-in dashboard.',
  },
  {
    title: 'Lightweight',
    desc: 'A lightweight native macOS app. No Electron bloat.',
  },
  {
    title: 'Open Source',
    desc: '100% open source. Read the code, contribute, or fork it.',
  },
]

const steps = [
  {
    num: '1',
    title: 'Press your hotkey',
    desc: 'A floating overlay appears so you know Buzz is listening.',
  },
  {
    num: '2',
    title: 'Speak naturally',
    desc: 'Talk at your normal pace. Buzz sends audio to OpenAI for transcription.',
  },
  {
    num: '3',
    title: 'Text appears',
    desc: 'Your transcript is inserted at the cursor position.',
  },
]

/* ─── component ──────────────────────────────────────────────────── */

function LandingPage() {
  return (
    <div className="min-h-screen">
      <Nav />
      <Hero />
      <Features />
      <HowItWorks />
      <Footer />
    </div>
  )
}

/* ─── nav ────────────────────────────────────────────────────────── */

function Nav() {
  return (
    <nav className="fixed top-0 left-0 right-0 z-50 bg-[#fafafa]/80 backdrop-blur-sm">
      <div className="max-w-3xl mx-auto px-6 h-14 flex items-center justify-between">
        <a
          href="#"
          className="text-sm font-semibold text-neutral-900 no-underline tracking-tight"
        >
          Buzz
        </a>
        <a
          href="https://github.com/SawyerHood/voice"
          target="_blank"
          rel="noopener noreferrer"
          className="text-sm text-neutral-500 hover:text-neutral-900 no-underline transition-colors"
        >
          GitHub
        </a>
      </div>
    </nav>
  )
}

/* ─── hero ───────────────────────────────────────────────────────── */

function Hero() {
  return (
    <header className="pt-28 pb-24 md:pt-36 md:pb-32">
      <div className="max-w-3xl mx-auto px-6">
        {/* Video placeholder */}
        <div className="mb-12 animate-fade-up">
          <div className="aspect-video w-full max-w-3xl mx-auto rounded-lg bg-gray-100 border border-gray-200 flex items-center justify-center">
            <span className="text-gray-400 text-sm">Product video coming soon</span>
          </div>
        </div>

        <div className="animate-fade-up" style={{ animationDelay: '100ms' }}>
          <h1 className="text-3xl sm:text-4xl font-semibold tracking-tight text-neutral-900 mb-4 leading-snug">
            Voice-to-text, instantly.
          </h1>

          <p className="text-lg text-neutral-500 mb-3 max-w-xl leading-relaxed">
            A tiny macOS menubar app that turns speech into text, right where
            your cursor is. Press a hotkey, talk, done.
          </p>

          <p className="text-sm text-neutral-400 mb-10">
            Open source and free to use.
          </p>

          <div className="flex items-center gap-3">
            <a
              href="#"
              className="inline-flex items-center gap-2 px-5 py-2.5 rounded-lg bg-neutral-900 text-white text-sm font-medium hover:bg-neutral-800 transition-colors no-underline"
            >
              <AppleIcon />
              Download for macOS
            </a>
            <a
              href="https://github.com/SawyerHood/voice"
              target="_blank"
              rel="noopener noreferrer"
              className="inline-flex items-center gap-2 px-5 py-2.5 rounded-lg text-neutral-600 text-sm font-medium border border-neutral-200 hover:border-neutral-300 transition-colors no-underline"
            >
              View source
            </a>
          </div>
        </div>
      </div>
    </header>
  )
}

/* ─── features ───────────────────────────────────────────────────── */

function Features() {
  return (
    <section id="features" className="py-20 md:py-28">
      <div className="max-w-3xl mx-auto px-6">
        <h2 className="text-2xl font-semibold text-neutral-900 mb-12 tracking-tight">
          Features
        </h2>

        <div className="grid grid-cols-1 sm:grid-cols-2 gap-x-16 gap-y-10">
          {features.map((f) => (
            <div key={f.title}>
              <h3 className="text-sm font-medium text-neutral-900 mb-1.5">
                {f.title}
              </h3>
              <p className="text-sm text-neutral-500 leading-relaxed">
                {f.desc}
              </p>
            </div>
          ))}
        </div>
      </div>
    </section>
  )
}

/* ─── how it works ───────────────────────────────────────────────── */

function HowItWorks() {
  return (
    <section id="how-it-works" className="py-20 md:py-28 border-t border-neutral-200">
      <div className="max-w-3xl mx-auto px-6">
        <h2 className="text-2xl font-semibold text-neutral-900 mb-12 tracking-tight">
          How it works
        </h2>

        <ol className="space-y-8">
          {steps.map((s) => (
            <li key={s.num} className="flex gap-4">
              <span className="text-sm font-medium text-amber-600 mt-0.5 shrink-0">
                {s.num}
              </span>
              <div>
                <h3 className="text-sm font-medium text-neutral-900 mb-1">
                  {s.title}
                </h3>
                <p className="text-sm text-neutral-500 leading-relaxed">
                  {s.desc}
                </p>
              </div>
            </li>
          ))}
        </ol>
      </div>
    </section>
  )
}

/* ─── footer ─────────────────────────────────────────────────────── */

function Footer() {
  return (
    <footer className="py-12 border-t border-neutral-200">
      <div className="max-w-3xl mx-auto px-6">
        <p className="text-xs text-neutral-400">
          Built by{' '}
          <a
            href="https://sawyerhood.com"
            target="_blank"
            rel="noopener noreferrer"
            className="text-neutral-500 hover:text-neutral-900 no-underline transition-colors"
          >
            Sawyer Hood
          </a>
        </p>
      </div>
    </footer>
  )
}

/* ─── icons ──────────────────────────────────────────────────────── */

function AppleIcon() {
  return (
    <svg className="w-4 h-4" viewBox="0 0 24 24" fill="currentColor" aria-hidden="true">
      <path d="M18.71 19.5c-.83 1.24-1.71 2.45-3.05 2.47-1.34.03-1.77-.79-3.29-.79-1.53 0-2 .77-3.27.82-1.31.05-2.3-1.32-3.14-2.53C4.25 17 2.94 12.45 4.7 9.39c.87-1.52 2.43-2.48 4.12-2.51 1.28-.02 2.5.87 3.29.87.78 0 2.26-1.07 3.8-.91.65.03 2.47.26 3.64 1.98-.09.06-2.17 1.28-2.15 3.81.03 3.02 2.65 4.03 2.68 4.04-.03.07-.42 1.44-1.38 2.83M13 3.5c.73-.83 1.94-1.46 2.94-1.5.13 1.17-.34 2.35-1.04 3.19-.69.85-1.83 1.51-2.95 1.42-.15-1.15.41-2.35 1.05-3.11z" />
    </svg>
  )
}
