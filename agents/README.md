# Agent skills

Skills for people who use or develop rs5cmd with an AI coding agent. They are plain Markdown for
the agent to read. Nothing in the rs5cmd binary, its build, or its tests loads this directory.

| Skill | For | Covers |
|---|---|---|
| [`rs5cmd-user`](skills/rs5cmd-user/SKILL.md) | Running rs5cmd | Install, credentials / endpoint / region, every command and global flag, the io_uring `--fast` path, the FUSE `mount`, recipes, JSON output for scripts, and known pitfalls |
| [`rs5cmd-developer`](skills/rs5cmd-developer/SKILL.md) | Changing rs5cmd | The containerized build and test workflow, the bench harness and `s3stub`, a code map, how to add a command or flag end to end, conventions and pitfalls, release and packaging, commit rules |

## Layout

```
agents/
  README.md
  skills/
    rs5cmd-user/
      SKILL.md                 start here; short, links to the files below
      references/              commands, configuration, --fast and mount, known issues
    rs5cmd-developer/
      SKILL.md
      references/              code map, adding a command, testing and bench, release
```

Each `SKILL.md` starts with YAML frontmatter holding a `name` (equal to its folder name) and a
`description` that says what the skill covers and when to use it. This is the
[Agent Skills](https://agentskills.io/specification) format. The agent loads the description
up front, the `SKILL.md` body when the skill applies, and a `references/` file only when the body
points to it.

## Installing

Copy or symlink a whole skill folder (not only `SKILL.md`; the `references/` files belong to it).

### Claude Code

```bash
# for every project on this machine
mkdir -p ~/.claude/skills
cp -r agents/skills/rs5cmd-user agents/skills/rs5cmd-developer ~/.claude/skills/

# or only for one checkout of some project
mkdir -p .claude/skills
cp -r /path/to/rs5cmd/agents/skills/rs5cmd-user .claude/skills/
```

Use `ln -s "$PWD/agents/skills/rs5cmd-user" ~/.claude/skills/` instead of `cp -r` if the installed
skill should follow this checkout. Claude Code then picks a skill by its description, or you can
call one directly with `/rs5cmd-user` or `/rs5cmd-developer`.

### Other agents

Any agent that implements the Agent Skills format can use these folders as they are. Put them in
that agent's skills directory. Many agents also read the shared locations `.agents/skills/` (inside
a project) and `~/.agents/skills/` (per user):

```bash
mkdir -p ~/.agents/skills
cp -r agents/skills/rs5cmd-user agents/skills/rs5cmd-developer ~/.agents/skills/
```

For an agent without skill support, point it at the file: name
`agents/skills/rs5cmd-user/SKILL.md` in the instructions file the agent already reads (for example
`AGENTS.md`), or paste the file into the conversation. The links inside a `SKILL.md` are relative
to its own folder.

## Keeping them accurate

The skills describe rs5cmd as of the commit they ship with. `rs5cmd <command> --help` and the
source are the authority; the skills say so and tell the agent to check.

- A change to the command line (a command, a flag, a default, output format) belongs in
  `rs5cmd-user` in the same change, mainly `references/commands.md`.
- A change to the build, the test services, the module layout, or a convention belongs in
  `rs5cmd-developer`.
- When a bug listed in `rs5cmd-user/references/known-issues.md` is fixed, delete its entry
  there, the matching line in `rs5cmd-user/SKILL.md`, and its row in the defect table at the end
  of `rs5cmd-developer/references/code-map.md`.

A stale skill is worse than none, because the agent repeats it with confidence.
