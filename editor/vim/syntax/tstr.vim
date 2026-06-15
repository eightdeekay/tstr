" Vim syntax file for tstr
" Language: tstr (HTTP API test runner)

if exists("b:current_syntax")
  finish
endif

" Comments
syn match tstrLineComment "//.*$"
syn region tstrBlockComment start="/\*" end="\*/"

" Arrows (dependency/export markers)
syn match tstrArrowIn "-->"
syn match tstrArrowOut "<--"

" Strings with interpolation
syn region tstrString start=/"/ skip=/\\./ end=/"/ contains=tstrEscape,tstrInterpolation
syn match tstrEscape /\\./ contained
syn match tstrInterpolation /{{[^}]\+}}/ contained

" Interpolation outside strings
syn match tstrInterpolation /{{[^}]\+}}/

" Regex literals (after ~ operators)
syn match tstrRegex /\(~?\s*\|!~\s*\|~\s*\)\/[^/]*\// contains=tstrRegexDelim
syn match tstrRegexDelim /\// contained

" Built-in functions
syn match tstrBuiltin /\$\.\(uuid\|string\|randEmail\|now\|log\)\>/
syn match tstrBuiltin /\$\.[a-zA-Z_][a-zA-Z0-9_]*/

" HTTP methods
syn match tstrHttpMethod /\<\(get\|post\|put\|patch\|delete\)\s*(/me=e-1

" Keywords
syn keyword tstrKeyword if else return
syn match tstrKeyword /\<js:/

" Status check
syn match tstrStatusCheck /?\s*\(\d\+\|\dxx\|[><=]\+\d\+\|\d\+-\d\+\)/

" Operators
syn match tstrOperator /==\|!=\|>=\|<=\|>\|</
syn match tstrOperator /&&\|||\||\|!~/
syn match tstrOperator /\~?\|~/

" Constants
syn keyword tstrConstant null true false

" Numbers
syn match tstrNumber /-\?\d\+\(\.\d\+\)\?/

" File references
syn match tstrFileRef /@[A-Za-z0-9_./-]\+/

" urlPrefix (special variable)
syn keyword tstrSpecialVar urlPrefix _response

" Linking to standard highlight groups
hi def link tstrLineComment Comment
hi def link tstrBlockComment Comment
hi def link tstrString String
hi def link tstrEscape SpecialChar
hi def link tstrInterpolation Special
hi def link tstrRegex String
hi def link tstrRegexDelim Delimiter
hi def link tstrBuiltin Function
hi def link tstrHttpMethod Keyword
hi def link tstrKeyword Keyword
hi def link tstrArrowIn Keyword
hi def link tstrArrowOut Keyword
hi def link tstrStatusCheck PreProc
hi def link tstrOperator Operator
hi def link tstrConstant Constant
hi def link tstrNumber Number
hi def link tstrFileRef String
hi def link tstrSpecialVar Identifier

let b:current_syntax = "tstr"
