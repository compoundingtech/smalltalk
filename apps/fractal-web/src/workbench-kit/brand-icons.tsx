// wf-local: third-party identification marks, not webfractal branding.
// Official sources and usage notes (checked 2026-10-02):
// OMP: github.com/can1357/oh-my-pi/assets/icon.svg (MIT; monochrome adaptation).
// Claude Spark / Anthropic symbol: anthropic.com/press-kit. Copyright/trademarks
// remain Anthropic's; press assets carry no open-source license or blanket grant.
// OpenAI Blossom: openai.com/brand, Blossom_Light.svg (standalone mark geometry).
// OpenAI marks usage terms apply: accurate service identification, no endorsement.
// OpenCode: opencode.ai/brand and anomalyco/opencode brand SVG (MIT).
// Copilot: primer/octicons/icons/copilot-16.svg (MIT; GitHub trademark retained).
// Z.ai: z.ai's linked z-cdn.chatglm.cn/z-ai/static/logo.svg (foreground geometry).
// Z.ai terms §V.2 require prior written consent even for non-public logo use:
// https://chat.z.ai/legal-agreement/terms-of-service. Permission is not established
// by this local prototype; obtain clearance before distributing these assets.
// xAI: official x.ai/legal/brand-guidelines; compact xAI geometry mirrored in
// anomalyco/opencode/packages/ui/src/assets/icons/provider/xai.svg (MIT mirror).
// xAI terms permit accurate reference, prohibit endorsement and altered marks.
// No mark here implies sponsorship. Geometry is preserved; paint follows the theme.
// MIT notices for the OMP, OpenCode and Octicons portions:
// Copyright (c) 2025 Mario Zechner; 2025-2026 Can Bölük; 2026 Stencil Labs, Inc.
// Copyright (c) 2025 opencode; Copyright (c) 2026 GitHub Inc.
/*
Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
*/

import * as stylex from '@stylexjs/stylex'
import type { ReactNode } from 'react'

const marks: Readonly<Record<string, { readonly viewBox: string; readonly shape: ReactNode }>> = {
  claude: {
    viewBox: '0 0 94 94',
    shape: (
      <>
        <path d="M18.7657 62.4437L37.1822 52.1167L37.4857 51.2122L37.1822 50.7085H36.2715L33.1852 50.5208L22.6615 50.2391L13.5545 49.8636L4.70044 49.3942L2.47428 48.9248L0.399902 46.1553L0.602281 44.794L2.47428 43.5266L5.15579 43.7613L11.0754 44.1837L19.98 44.794L26.4055 45.1695L35.9679 46.1553H37.4857L37.6881 45.545L37.1822 45.1695L36.7774 44.794L27.5692 38.5508L17.6021 31.9791L12.3908 28.1769L9.60812 26.2524L8.19147 24.4686L7.58433 20.5256L10.1141 17.7091L13.5545 17.9438L14.4146 18.1785L17.9056 20.8542L25.343 26.6279L35.0572 33.7629L36.4739 34.9364L37.0443 34.5514L37.1316 34.2792L36.4739 33.1996L31.212 23.6706L25.596 13.9539L23.0663 9.91695L22.4086 7.52296C22.1538 6.51831 22.0038 5.68714 22.0038 4.65957L24.8877 0.716544L26.5067 0.200195L30.4025 0.716544L32.0215 2.12477L34.4501 7.66379L38.3458 16.3478L44.4172 28.1769L46.188 31.6975L47.1493 34.9364L47.5035 35.9222H48.1106V35.3589L48.6166 28.6933L49.5273 20.5256L50.438 10.0108L50.7415 7.05356L52.2088 3.48605L55.1433 1.56148L57.42 2.64112L59.292 5.31674L59.039 7.05356L57.926 14.2824L55.7504 25.5952L54.3337 33.1996H55.1433L56.1046 32.2138L59.9497 27.1442L66.3752 19.0704L69.2085 15.8784L72.5478 12.3579L74.6728 10.668H78.7203L81.6548 15.0804L80.3394 19.6337L76.1906 24.8911L72.7502 29.3504L67.8172 35.9595L64.7562 41.2734L65.0307 41.7118L65.7681 41.6489L76.8989 39.255L82.9197 38.1753L90.1041 36.9549L93.3422 38.457L93.6963 40.006L92.4315 43.151L84.7411 45.0287L75.7353 46.8594L62.3244 50.0164L62.1759 50.1358L62.3512 50.3958L68.399 50.9432L70.9794 51.084H77.3037L89.0922 51.9759L92.1785 53.9944L93.9999 56.4822L93.6963 58.4068L88.9404 60.8008L82.5655 59.2987L67.6401 55.7312L62.5301 54.4638H61.8217V54.8862L66.0717 59.064L73.9139 66.1051L83.6786 75.2116L84.1845 77.4648L82.9197 79.2485L81.6042 79.0608L73.0032 72.5829L69.6639 69.6726L62.1759 63.3356H61.67V63.9928L63.3902 66.5276L72.5478 80.2812L73.0032 84.5059L72.3454 85.8672L69.9675 86.7121L67.3871 86.2427L61.9735 78.6852L56.4587 70.2359L52.0064 62.6315L51.4687 62.971L48.8189 91.2654L47.6047 92.7206L44.7714 93.8002L42.3934 92.0164L41.1286 89.1061L42.3934 83.3324L43.9113 75.8219L45.1255 69.8604L46.2386 62.4437L46.9184 59.9661L46.8583 59.8003L46.3153 59.8916L40.7238 67.5603L32.2239 79.0608L25.4948 86.2427L23.8758 86.8999L21.0931 85.4447L21.3461 82.863L22.9145 80.5629L32.2239 68.7338L37.8399 61.3641L41.4594 57.1337L41.4242 56.5218L41.2244 56.5048L16.489 72.6299L12.0873 73.1932L10.1647 71.4094L10.4176 68.4991L11.3283 67.5603L18.7657 62.4437Z" />
      </>
    ),
  },
  anthropic: {
    viewBox: '0 0 92 64',
    shape: (
      <>
        <path d="M66.4915 0H52.5029L78.0115 64H92.0001L66.4915 0Z" />
        <path d="M26.08 0L0.571472 64H14.8343L20.0512 50.56H46.7374L51.9543 64H66.2172L40.7086 0H26.08ZM24.6647 38.6743L33.3943 16.1829L42.1239 38.6743H24.6647Z" />
      </>
    ),
  },
  copilot: {
    viewBox: '0 0 16 16',
    shape: (
      <>
        <path d="M7.998 15.035c-4.562 0-7.873-2.914-7.998-3.749V9.338c.085-.628.677-1.686 1.588-2.065.013-.07.024-.143.036-.218.029-.183.06-.384.126-.612-.201-.508-.254-1.084-.254-1.656 0-.87.128-1.769.693-2.484.579-.733 1.494-1.124 2.724-1.261 1.206-.134 2.262.034 2.944.765.05.053.096.108.139.165.044-.057.094-.112.143-.165.682-.731 1.738-.899 2.944-.765 1.23.137 2.145.528 2.724 1.261.566.715.693 1.614.693 2.484 0 .572-.053 1.148-.254 1.656.066.228.098.429.126.612.012.076.024.148.037.218.924.385 1.522 1.471 1.591 2.095v1.872c0 .766-3.351 3.795-8.002 3.795Zm0-1.485c2.28 0 4.584-1.11 5.002-1.433V7.862l-.023-.116c-.49.21-1.075.291-1.727.291-1.146 0-2.059-.327-2.71-.991A3.222 3.222 0 0 1 8 6.303a3.24 3.24 0 0 1-.544.743c-.65.664-1.563.991-2.71.991-.652 0-1.236-.081-1.727-.291l-.023.116v4.255c.419.323 2.722 1.433 5.002 1.433ZM6.762 2.83c-.193-.206-.637-.413-1.682-.297-1.019.113-1.479.404-1.713.7-.247.312-.369.789-.369 1.554 0 .793.129 1.171.308 1.371.162.181.519.379 1.442.379.853 0 1.339-.235 1.638-.54.315-.322.527-.827.617-1.553.117-.935-.037-1.395-.241-1.614Zm4.155-.297c-1.044-.116-1.488.091-1.681.297-.204.219-.359.679-.242 1.614.091.726.303 1.231.618 1.553.299.305.784.54 1.638.54.922 0 1.28-.198 1.442-.379.179-.2.308-.578.308-1.371 0-.765-.123-1.242-.37-1.554-.233-.296-.693-.587-1.713-.7Z" />
        <path d="M6.25 9.037a.75.75 0 0 1 .75.75v1.501a.75.75 0 0 1-1.5 0V9.787a.75.75 0 0 1 .75-.75Zm4.25.75v1.501a.75.75 0 0 1-1.5 0V9.787a.75.75 0 0 1 1.5 0Z" />
      </>
    ),
  },
  xai: {
    viewBox: '0 0 40 40',
    shape: (
      <>
        <path d="M12.4579 15.6036L26.1529 35H20.0656L6.37059 15.6036H12.4579ZM12.4524 26.3764L15.4974 30.6909L12.4551 35H6.36377L12.4524 26.3764ZM33.6365 7.15727V35H28.647V14.2236L33.6365 7.15727ZM33.6365 5L20.0656 24.2205L17.0206 19.9073L27.5451 5H33.6365Z" />
      </>
    ),
  },
  openai: {
    viewBox: '780 225 262 272',
    shape: (
      <>
        <path d="M872.176 325.027V299.869C872.176 297.751 872.971 296.161 874.825 295.102L925.406 265.974C932.29 262.002 940.5 260.148 948.973 260.148C980.75 260.148 1000.88 284.777 1000.88 310.992C1000.88 312.845 1000.88 314.963 1000.61 317.083L948.178 286.364C945.001 284.512 941.822 284.512 938.645 286.364L872.176 325.027ZM990.283 423.008V362.894C990.283 359.185 988.694 356.538 985.516 354.684L919.048 316.023L940.763 303.575C942.617 302.517 944.206 302.517 946.058 303.575L996.639 332.705C1011.2 341.179 1021 359.185 1021 376.662C1021 396.788 1009.09 415.325 990.283 423.005V423.008ZM856.553 370.045L834.838 357.334C832.986 356.277 832.19 354.688 832.19 352.568V294.311C832.19 265.975 853.905 244.525 883.301 244.525C894.423 244.525 904.748 248.234 913.49 254.853L861.321 285.042C858.146 286.896 856.555 289.544 856.555 293.252V370.048L856.553 370.045ZM903.292 397.055L872.176 379.578V342.506L903.292 325.029L934.407 342.506V379.578L903.292 397.055ZM923.286 477.561C912.163 477.561 901.837 473.852 893.097 467.233L945.264 437.042C948.441 435.19 950.03 432.541 950.03 428.832V352.037L972.011 364.748C973.865 365.805 974.66 367.395 974.66 369.515V427.772C974.66 456.107 952.679 477.558 923.286 477.558V477.561ZM860.525 418.507L809.944 389.377C795.378 380.903 785.582 362.897 785.582 345.42C785.582 325.029 797.763 306.757 816.563 299.077V359.454C816.563 363.163 818.154 365.81 821.33 367.664L887.535 406.06L865.82 418.507C863.967 419.565 862.377 419.565 860.525 418.507ZM857.614 461.936C827.689 461.936 805.71 439.426 805.71 411.621C805.71 409.503 805.976 407.385 806.238 405.265L858.405 435.456C861.582 437.308 864.763 437.308 867.938 435.456L934.407 397.058V422.215C934.407 424.335 933.612 425.924 931.758 426.982L881.179 456.112C874.293 460.084 866.083 461.936 857.611 461.936H857.614ZM923.286 493.447C955.329 493.447 982.073 470.674 988.167 440.485C1017.83 432.804 1036.89 404.999 1036.89 376.665C1036.89 358.128 1028.95 340.122 1014.65 327.145C1015.97 321.584 1016.77 316.023 1016.77 310.463C1016.77 272.596 986.048 244.259 950.562 244.259C943.413 244.259 936.528 245.316 929.644 247.702C917.725 236.049 901.307 228.635 883.301 228.635C851.258 228.635 824.513 251.408 818.42 281.597C788.761 289.278 769.694 317.083 769.694 345.417C769.694 363.955 777.638 381.961 791.938 394.937C790.613 400.498 789.819 406.06 789.819 411.62C789.819 449.487 820.538 477.824 856.024 477.824C863.172 477.824 870.058 476.766 876.943 474.38C888.859 486.033 905.278 493.447 923.286 493.447Z" />
      </>
    ),
  },
  omp: {
    viewBox: '0 0 120 90',
    shape: (
      <>
        <rect x="10" y="8" width="100" height="12" rx="2" />
        <rect x="25" y="20" width="12" height="62" rx="2" />
        <rect x="75" y="20" width="12" height="45" rx="2" />
        <path
          fillRule="evenodd"
          d="M74 55h14a3 3 0 0 1 3 3v10a3 3 0 0 1-3 3H74a3 3 0 0 1-3-3V58a3 3 0 0 1 3-3Zm3 4a1 1 0 0 0-1 1v6a1 1 0 0 0 1 1h1a1 1 0 0 0 1-1v-6a1 1 0 0 0-1-1Zm6 0a1 1 0 0 0-1 1v6a1 1 0 0 0 1 1h1a1 1 0 0 0 1-1v-6a1 1 0 0 0-1-1Z"
        />
      </>
    ),
  },
  opencode: {
    viewBox: '0 0 240 300',
    shape: (
      <>
        <path d="M180 240H60V120H180V240Z" opacity="0.35" />
        <path fillRule="evenodd" d="M180 60H60V240H180V60ZM240 300H0V0H240V300Z" />
      </>
    ),
  },
  zai: {
    viewBox: '0 0 30 30',
    shape: (
      <>
        <path d="M15.47 7.1l-1.3 1.85c-.2.29-.54.47-.9.47h-7.1V7.09L15.47 7.1Z" />
        <polygon points="24.3,7.1 13.14,22.91 5.7,22.91 16.86,7.1" />
        <path d="m14.53 22.91 1.31-1.86c.2-.29.54-.47.9-.47h7.09v2.33Z" />
      </>
    ),
  },
}

const brands: Readonly<Record<string, { readonly mark: string; readonly label: string }>> = {
  omp: { mark: 'omp', label: 'oh-my-pi' },
  'oh-my-pi': { mark: 'omp', label: 'oh-my-pi' },
  claude: { mark: 'claude', label: 'Claude' },
  'claude-code': { mark: 'claude', label: 'Claude Code' },
  'claude code': { mark: 'claude', label: 'Claude Code' },
  anthropic: { mark: 'anthropic', label: 'Anthropic' },
  openai: { mark: 'openai', label: 'OpenAI' },
  codex: { mark: 'openai', label: 'Codex' },
  'codex cli': { mark: 'openai', label: 'Codex CLI' },
  'openai-codex': { mark: 'openai', label: 'OpenAI Codex' },
  opencode: { mark: 'opencode', label: 'OpenCode' },
  'opencode-go': { mark: 'opencode', label: 'OpenCode Go' },
  copilot: { mark: 'copilot', label: 'GitHub Copilot' },
  'github-copilot': { mark: 'copilot', label: 'GitHub Copilot' },
  zai: { mark: 'zai', label: 'Z.ai' },
  'z.ai': { mark: 'zai', label: 'Z.ai' },
  zhipu: { mark: 'zai', label: 'Zhipu / Z.ai' },
  zhipuai: { mark: 'zai', label: 'Zhipu / Z.ai' },
  xai: { mark: 'xai', label: 'xAI' },
  'xai-oauth': { mark: 'xai', label: 'xAI' },
}

const styles = stylex.create({
  icon: {
    display: 'inline-flex',
    alignItems: 'center',
    justifyContent: 'center',
    flexShrink: 0,
    verticalAlign: 'middle',
    lineHeight: 0,
    paddingInline: 2,
  },
})

/** The only brand renderer: unknown gateway ids remain readable rather than disappearing. */
export const HarnessIcon = ({ id, size = 14 }: { readonly id: string; readonly size?: number }) => {
  const brand = brands[id.toLowerCase()]
  const mark = brand === undefined ? undefined : marks[brand.mark]
  if (brand === undefined || mark === undefined) return <>{id}</>
  return (
    <span title={brand.label} {...stylex.props(styles.icon)}>
      <svg
        role="img"
        aria-label={brand.label}
        width={size}
        height={size}
        viewBox={mark.viewBox}
        fill="currentColor"
        focusable="false"
      >
        <title>{brand.label}</title>
        {mark.shape}
      </svg>
    </span>
  )
}

// Only standalone names, not model ids, transcript prose, or substrings of unknown ids.
const brandNames =
  /(github-copilot|openai-codex|claude-code|claude code|codex cli|oh-my-pi|opencode-go|xai-oauth|anthropic|opencode|copilot|zhipuai|openai|claude|codex|zhipu|z\.ai|zai|xai|omp)(?=$|[\s·/])/giu

/**
 * Marks known brand names inside free text that carries no structured identity (e.g. a terminal
 * title set by the program). Slots with a structured harness/provider id render `HarnessIcon`
 * from that id instead of re-parsing prose. Underlying ids and search text stay intact.
 */
export const BrandText = ({ text }: { readonly text: string | undefined }) => {
  if (text === undefined) return null
  const parts: ReactNode[] = []
  let end = 0
  for (const match of text.matchAll(brandNames)) {
    const start = match.index
    if (start > 0 && !/[\s·/]/u.test(text[start - 1] ?? '')) continue
    parts.push(text.slice(end, start), <HarnessIcon key={start} id={match[0]} />)
    end = start + match[0].length
  }
  parts.push(text.slice(end))
  return <>{parts}</>
}
