# Claude Code Guidelines

## Import Resolution Guidelines

**Use fully qualified absolute imports for all module resolution**:
- Use `from package.module import Class` instead of relative imports `from .module import Class`
- This ensures clear import paths and avoids ambiguity
- Example: `from running_process.output_formatter import OutputFormatter` not `from .output_formatter import OutputFormatter`
- Apply this rule to ALL imports within the package, including internal module imports

## Subprocess Command Guidelines

**Never use str.join() to convert subprocess command lists to command strings**:
- Use `subprocess.list2cmdline()` instead of `str.join()` for proper shell escaping
- This ensures proper handling of arguments containing spaces, quotes, and special characters
- Example: `subprocess.list2cmdline(command)` not `' '.join(command)`
- This prevents command injection vulnerabilities and ensures cross-platform compatibility