# RFC 7117 - 2021-07-27 - VRL string interpolation

VRL needs a better way to format strings. Currently the only way to do this is
to concatenate strings, which can get unwieldly.

## Scope

This RFC discusses creating a new string type within VRL that can use template
literals, also known as string interpolation, to format strings.

## Pain

Currently the way to create strings is through either string concatenation or
the `join` function.

Syntactically this is unwieldly. It requires extra key presses and the code
created doesn't necessarily give an instant idea of what the resulting string
will look like. Thus the true intent behind the code is obfuscated, which can
result in bugs.

## User Experience

We will be loosely basing our format strings on Pythons [f-strings](https://peps.python.org/pep-0498/).

To format a string there will be a string type denoted with the prefix `f'`.

Within that string, it is possible to embed VRL expressions by surrounding them
with `{..}`. VRL will evaluate the expressions and will call `to_string` on
that expression to return the text representation.

```coffee
f'The message is { .message } created at { .timestamp }'
```

If you wish to actually insert a `{` into a string, a double '{{' will be needed.

```coffee
f'Here is a curly brace -> {{'
```

Since this is a new string type there are no backward compatibility issues.

### Format strings

The format can be specified by adding format strings after a `:` in the string.

For example to format date the following would be valid:

```coffee
ts = t'2020-10-21T16:00:00Z'
f'The time is {ts : %v %R}'
```

### Errors

The question arises about what we should do if an error occurs.

I would argue that we do not want an f string to be fallible as this would
cumbersome to the experience of using VRL. If an error occurs the error text is
output. For example:

```coffee
f'This is some json { parse_json(.thing) }'
# This is some json function call error for "parse_json" at (0:18): unable to parse json: expected ident at line 1 column 2
```

Another source of error would be if the format string is specified for a different
type - for example using date format strings when the type is an integer.

If this occurs, we just ignore the format string and display the unformatted value.

```coffee
thing = 2
f'The date is {thing: %v %R}.'
# The date is 2.
```

## Implementation

This new string type can be considered as syntactic sugar for string
concatenation.

The VRL parser will take a template literal string such as:

```
i'The message is {{ .message }} created at {{ .timestamp: %v %R }}'
```

and create an AST identical to the AST for the following expression:

```
s'The message is ' +
as_string(.message) +
s' created at ' +
as_string(.timestamp, format: "%v %R")
```

`as_string` is a new function that will convert any type to a string and potentially
apply format strings. Objects will be json encoded.

## Rationale

String interpolation or string formats are prevalent in modern programming
languages. Users have an expectation that this feature will be available.

String formatting is a common task within VRL. Currently the process involves
string concatenation. This works, but the code required to do this does not
create an immediately apparent representation of what the string may look like.

There is little impact of not doing this beyond requiring users to use a less
elegant form for string creation.

## Prior Art

- Template strings are used within certain fields within Vector.
- Many programming languages offer string interpolation.
  - [Python](https://peps.python.org/pep-0498/)
  - [Javascript](https://developer.mozilla.org/en-US/docs/Web/JavaScript/Reference/Template_literals)
  - [Ruby](http://ruby-for-beginners.rubymonstas.org/bonus/string_interpolation.html)

## Drawbacks

This does add additional code complexity and an maintenance burden to VRL.

## Alternatives

### Format Strings

One alternative is to create a `sprintf` function. `sprintf` takes one parameter
that represents the format string. Within the format string if a format tag is
found, the function will take the next parameter passed and will format that
parameter according to the tag and will embed the resulting text in that
position.

For example:

```
sprintf("The message is %s created at %t", .message, .timestamp)
```

will return

```
The message is the message created at Tue, 27 Jul 2021 10:10:01 +0000
```

The advantages of this method is that it does not require any changes to the
VRL compiler, all changes are isolated to a single function. Also it provides
a way to influence the formatting of the parameters.

Downsides are that the format strings are a hidden DSL themselves and there is
a cognitive overhead involved in maintaining the position of the format tags
within the string and the parameters passed to the function.

### Extended template strings

The advantage of format strings over string interpolation is that format strings
provide for parameters to indicate how numbers could be formatted.

For example, `sprintf("%.2f", 3.14159)` will return `3.14`. With simple string
interpolation `i'{{ 3.14159 }}'` there is no way to control the number of
decimals output.

We could extend the format strings to allow for any format parameters to be
added after a `|` character in the template string. So
`i'{{ 3.14159 | decimals: 2 }}'` would result in the number formatted to 2
decimal places - `3.14`.

### Fallibility

F-strings could be fallible and their use could require any errors to be handled.

```coffee
f'The date is { .date: %v %R }' ?? "invalid date"
```

Or

```coffee
f'The date is { timestamp!(.date): %v %R }'
```


## Outstanding Questions

- Is there a better prefix than `i` for string interpolation?

## Plan Of Attack

Incremental steps to execute this change. These will be converted to issues after the RFC is approved:

- [ ] Submit a PR with spike-level code _roughly_ demonstrating the change.
- [ ] Incremental change #1
- [ ] Incremental change #2
- [ ] ...

Note: This can be filled out during the review process.
