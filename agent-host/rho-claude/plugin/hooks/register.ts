import type { On, PromptComposeSection } from 'claude-code'

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
    // Rho's prompt covers what Claude Code's other sections say, or
    // contradicts it. `lean_body` is shared, so it goes ahead of the rest.
    const kept = sections.flatMap(section =>
      section.id === 'lean_body' ? important(section) : [],
    )
    return { sections: [...kept, { id: 'rho:instructions', text, scope: 'session' }] }
  })

  // Claude Code's environment note names its own working directory, which
  // can differ from the agent's, and the token count never changes.
  on('prompt.attachment', { type: 'environment' }, () => ({ text: null }))
  on('prompt.attachment', { type: 'total_tokens_reminder' }, () => ({ text: null }))

  // A compaction drops the transcript that showed the agent its skills.
  // Rho lists their directories in `rho-skills`; each `SKILL.md` is read
  // now and follows the summary, as Rho's own runtime does it.
  on('session.compact', async ($, e, next) => {
    const result = await next(e)
    if (result.skip !== undefined) return result
    const home = await $.env.get('CLAUDE_CONFIG_DIR')
    const dirs = (await $.fs.read(`${home}/rho-skills`)).split('\n').filter(dir => dir !== '')
    if (dirs.length === 0) return result
    const skills = await Promise.all(dirs.map(async dir => {
      try {
        const text = (await $.fs.read(`${dir}/SKILL.md`)).trimEnd()
        return `<skill path="${dir}">\n${text}\n</skill>`
      } catch (error) {
        return `<skill path="${dir}" error="${String(error)}" />`
      }
    }))
    const text = `Your loaded skills, as they are now. Keep following them:\n\n${skills.join('\n\n')}`
    return { ...result, messages: [...result.messages, { role: 'user', text, toolUses: [] }] }
  })

  on('prompt.context', async ($, e, next) => {
    const { blocks } = await next(e)
    return {
      blocks: blocks.filter(block => block.name !== 'claudeMd' && block.name !== 'userEmail'),
      instructionFiles: [],
    }
  })
}
