#pragma once

#include <stddef.h>

#include "tree_sitter/api.h"

/// Returns the linked-in parser for a symbol name, or NULL when absent.
const TSLanguage *nvim_ts_get_parser(const char *lang);

/// Returns the parser name at index, or NULL after the last registry entry.
const char *nvim_ts_get_parser_name(size_t index);
