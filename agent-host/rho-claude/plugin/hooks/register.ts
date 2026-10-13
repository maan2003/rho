import type { On, PromptComposeSection } from 'claude-code'

/**
 * Claude Code sections Rho replaces: the Skill tool and `/code-review`,
 * Claude's own memory, and model IDs and `/fast`. Sections this list does
 * not name, such as per-model guidance, reach the model as Claude Code
 * wrote them.
 */
const DROPPED = new Set(['session_guidance', 'memory', 'env_info_simple'])

/**
 * Of `lean_body`, only its `IMPORTANT:` paragraphs: its intro and its
 * `# Harness` list describe a terminal agent with file tools, and Rho's
 * own prompt says who the agent is and how it acts.
 */
function important(section: PromptComposeSection): PromptComposeSection[] {
  const text = section.text
    .split(/\n{2,}/)
    .filter(paragraph => paragraph.trimStart().startsWith('IMPORTANT:'))
    .join('\n\n')
  return text === '' ? [] : [{ ...section, text }]
}

/**
 * Rho mounts the agent's prompt as the account's `CLAUDE.md`, which Claude
 * Code sends in the first user message. Here the same file becomes the
 * system prompt and the user message loses it; where this module does not
 * load, the agent still reads the prompt there.
 */
export function register(on: On): void {
  on('prompt.compose', async ($, e, next) => {
    const { sections } = await next(e)
    const home = await $.env.get('CLAUDE_CONFIG_DIR')
    const text = await $.fs.read(`${home}/CLAUDE.md`)
    const kept = sections.flatMap(section =>
      section.id === 'lean_body'
        ? important(section)
        : DROPPED.has(section.id)
          ? []
          : [section],
    )
    const rho: PromptComposeSection = { id: 'rho:instructions', text, scope: 'session' }
    return {
      sections: [
        ...kept.filter(section => section.scope === 'shared'),
        rho,
        ...kept.filter(section => section.scope === 'session'),
      ],
    }
  })

  on('prompt.context', async ($, e, next) => {
    const { blocks } = await next(e)
    return {
      blocks: blocks.filter(block => block.name !== 'claudeMd' && block.name !== 'userEmail'),
      instructionFiles: [],
    }
  })
}
