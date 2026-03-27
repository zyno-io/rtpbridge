import { defineConfig } from 'vitepress'

export default defineConfig({
  title: 'rtpbridge',
  description: 'RTP media routing server in Rust',
  base: '/rtpbridge/',
  themeConfig: {
    nav: [
      { text: 'Guide', link: '/guide/getting-started' },
      { text: 'Protocol', link: '/protocol/overview' },
      { text: 'Reference', link: '/reference/codecs' },
      { text: 'GitHub', link: 'https://github.com/zyno-io/rtpbridge' }
    ],
    sidebar: [
      {
        text: 'Guide',
        items: [
          { text: 'Getting Started', link: '/guide/getting-started' },
          { text: 'Architecture', link: '/guide/architecture' },
          { text: 'Configuration', link: '/guide/configuration' },
          { text: 'Deployment', link: '/guide/deployment' },
          { text: 'Troubleshooting', link: '/guide/troubleshooting' },
          { text: 'Observability', link: '/guide/observability' },
          { text: 'Performance Tuning', link: '/guide/performance' },
          { text: 'Disaster Recovery', link: '/guide/disaster-recovery' }
        ]
      },
      {
        text: 'Control Protocol',
        items: [
          { text: 'Overview', link: '/protocol/overview' },
          { text: 'Sessions', link: '/protocol/session' },
          { text: 'Endpoints', link: '/protocol/endpoints' },
          { text: 'File Playback', link: '/protocol/file-playback' },
          { text: 'DTMF', link: '/protocol/dtmf' },
          { text: 'Recording', link: '/protocol/recording' },
          { text: 'VAD', link: '/protocol/vad' },
          { text: 'Statistics', link: '/protocol/stats' },
          { text: 'Events', link: '/protocol/events' }
        ]
      },
      {
        text: 'Reference',
        items: [
          { text: 'Codecs', link: '/reference/codecs' },
          { text: 'SRTP', link: '/reference/srtp' }
        ]
      }
    ],
    socialLinks: [
      { icon: 'github', link: 'https://github.com/zyno-io/rtpbridge' }
    ]
  }
})
