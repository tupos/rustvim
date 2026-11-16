#ifdef __EMSCRIPTEN__
#include <stddef.h>
#include <string.h>

#include "static_ts_registry.h"

extern const TSLanguage *tree_sitter_lua(void);
extern const TSLanguage *tree_sitter_c(void);
extern const TSLanguage *tree_sitter_diff(void);
extern const TSLanguage *tree_sitter_vim(void);
extern const TSLanguage *tree_sitter_markdown(void);
extern const TSLanguage *tree_sitter_markdown_inline(void);
extern const TSLanguage *tree_sitter_query(void);
extern const TSLanguage *tree_sitter_vimdoc(void);

typedef const TSLanguage *(*ts_parser_fn)(void);

typedef struct {
  const char *name;
  ts_parser_fn fn;
} TsParserEntry;

static TsParserEntry parsers[] = {
  { "lua", tree_sitter_lua },
  { "c", tree_sitter_c },
  { "diff", tree_sitter_diff },
  { "vim", tree_sitter_vim },
  { "markdown", tree_sitter_markdown },
  { "markdown_inline", tree_sitter_markdown_inline },
  { "query", tree_sitter_query },
  { "vimdoc", tree_sitter_vimdoc },
};

// Returns a registry name, or NULL after the last entry.
const char *nvim_ts_get_parser_name(size_t index)
{
  if (index >= sizeof(parsers) / sizeof(parsers[0])) {
    return NULL;
  }
  return parsers[index].name;
}

const TSLanguage *nvim_ts_get_parser(const char *lang)
{
  for (size_t i = 0; i < sizeof(parsers) / sizeof(parsers[0]); i++) {
    if (strcmp(lang, parsers[i].name) == 0) {
      return parsers[i].fn();
    }
  }
  return NULL;
}
#endif
