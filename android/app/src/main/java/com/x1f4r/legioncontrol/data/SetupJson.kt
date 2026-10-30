package com.x1f4r.legioncontrol.data

import kotlinx.serialization.ExperimentalSerializationApi
import kotlinx.serialization.json.JsonArray
import kotlinx.serialization.json.JsonElement
import kotlinx.serialization.json.JsonNull
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.JsonPrimitive
import kotlinx.serialization.json.JsonUnquotedLiteral

/**
 * A JSON tree that keeps what a setup document must not lose: the order of members and the spelling
 * of numbers.
 *
 * A merge has to give back everything it did not decide about exactly as it found it, and parsing
 * `22.0` into a double and printing it again is precisely how one client used to rewrite another
 * peer's document. So numbers stay lexemes here, members stay in the order they were written, and
 * strings are decoded exactly as `JSON.parse` decodes them. Nodes are mutable, because a merge
 * builds its result by changing a copy of one side in place, as the reference in
 * `contract/tools/setup-sync.mjs` does; [copy] is how a side is kept intact.
 *
 * The parser is a strict one on purpose. The lenient reader the rest of the app uses for agent
 * replies takes unquoted strings and raw control characters, and a setup document that one device
 * accepts and the next refuses is how replication stops without anybody noticing (contract section
 * 12 says what a document is, and `JSON.parse` is its reading).
 */
sealed class SetupJson {
    /** Members in the order they were written. A name appears once. */
    class Object(val members: MutableList<Pair<String, SetupJson>> = mutableListOf()) : SetupJson() {
        operator fun get(name: String): SetupJson? = members.firstOrNull { it.first == name }?.second

        fun has(name: String): Boolean = members.any { it.first == name }

        /** Sets a member where it stands, or appends it when there is none of that name. */
        operator fun set(name: String, value: SetupJson) {
            val index = members.indexOfFirst { it.first == name }
            if (index >= 0) members[index] = name to value else members += name to value
        }

        fun remove(name: String) {
            members.removeAll { it.first == name }
        }
    }

    class Array(val items: MutableList<SetupJson> = mutableListOf()) : SetupJson()

    class Text(val value: String) : SetupJson()

    /** A number as it was written. Two spellings of one value are equal ([JsonEquality]), never rewritten. */
    class Number(val lexeme: String) : SetupJson()

    class Bool(val value: Boolean) : SetupJson()

    data object Null : SetupJson()

    /** A deep copy, so a change to it never reaches the tree it was taken from. */
    fun copy(): SetupJson = when (this) {
        is Object -> Object(members.mapTo(mutableListOf()) { (name, value) -> name to value.copy() })
        is Array -> Array(items.mapTo(mutableListOf()) { it.copy() })
        is Text, is Number, is Bool, Null -> this
    }

    /**
     * The document text of this tree: two-space indentation, LF, one final newline, the layout
     * `JSON.stringify(value, null, 2)` gives, with every number written exactly as it was read.
     */
    fun print(): String = StringBuilder().also { printTo(it, "") }.append('\n').toString()

    private fun printTo(out: StringBuilder, indent: String) {
        val inner = "$indent  "
        when (this) {
            is Object -> if (members.isEmpty()) {
                out.append("{}")
            } else {
                out.append("{\n")
                members.forEachIndexed { index, (name, value) ->
                    if (index > 0) out.append(",\n")
                    out.append(inner).append(quote(name)).append(": ")
                    value.printTo(out, inner)
                }
                out.append('\n').append(indent).append('}')
            }

            is Array -> if (items.isEmpty()) {
                out.append("[]")
            } else {
                out.append("[\n")
                items.forEachIndexed { index, value ->
                    if (index > 0) out.append(",\n")
                    out.append(inner)
                    value.printTo(out, inner)
                }
                out.append('\n').append(indent).append(']')
            }

            is Text -> out.append(quote(value))
            is Number -> out.append(lexeme)
            is Bool -> out.append(if (value) "true" else "false")
            Null -> out.append("null")
        }
    }

    /**
     * This tree in the shape the rest of the app reads, keeping member order and number lexemes:
     * kotlinx keeps both, so a document read here and edited there is printed back unchanged.
     */
    @OptIn(ExperimentalSerializationApi::class)
    fun toElement(): JsonElement = when (this) {
        is Object -> JsonObject(members.associateTo(LinkedHashMap()) { (name, value) -> name to value.toElement() })
        is Array -> JsonArray(items.map { it.toElement() })
        is Text -> JsonPrimitive(value)
        is Number -> JsonUnquotedLiteral(lexeme)
        is Bool -> JsonPrimitive(value)
        Null -> JsonNull
    }

    companion object {
        /** A kotlinx tree as this one. A number keeps the text it holds, which is the lexeme it was read as. */
        fun of(element: JsonElement): SetupJson = when (element) {
            is JsonObject -> Object(element.entries.mapTo(mutableListOf()) { (name, value) -> name to of(value) })
            is JsonArray -> Array(element.mapTo(mutableListOf()) { of(it) })
            JsonNull -> Null
            is JsonPrimitive -> when {
                element.isString -> Text(element.content)
                element.content == "true" || element.content == "false" -> Bool(element.content == "true")
                else -> Number(element.content)
            }
        }

        /**
         * A string as `JSON.stringify` writes it: `"` and `\` escaped, the short escapes for
         * backspace, form feed, line feed, carriage return and tab, `\u00xx` for every other control
         * character and for a lone surrogate, and everything else as it is.
         */
        fun quote(value: String): String {
            val out = StringBuilder(value.length + 2).append('"')
            var index = 0
            while (index < value.length) {
                val c = value[index]
                when {
                    c == '"' -> out.append("\\\"")
                    c == '\\' -> out.append("\\\\")
                    c == '\b' -> out.append("\\b")
                    c == '\u000C' -> out.append("\\f")
                    c == '\n' -> out.append("\\n")
                    c == '\r' -> out.append("\\r")
                    c == '\t' -> out.append("\\t")
                    c < ' ' -> out.append(unicodeEscape(c))
                    c.isHighSurrogate() && index + 1 < value.length && value[index + 1].isLowSurrogate() -> {
                        out.append(c).append(value[index + 1])
                        index += 1
                    }

                    c.isSurrogate() -> out.append(unicodeEscape(c))
                    else -> out.append(c)
                }
                index += 1
            }
            return out.append('"').toString()
        }

        private fun unicodeEscape(c: Char): String = "\\u" + c.code.toString(16).padStart(4, '0')
    }
}

/**
 * Reading JSON text the way `JSON.parse` does, into a [SetupJson] tree.
 *
 * Whitespace is space, tab, line feed and carriage return and nothing else; a string holds no raw
 * control character and only the escapes JSON has; a number is the JSON grammar and nothing a
 * language's own number parser forgives, such as a leading zero or a bare `.5`. Anything else is
 * [Malformed].
 */
object SetupJsonParser {

    /** The text is not exactly one JSON value. */
    class Malformed(message: String) : IllegalArgumentException(message)

    /** A tree, and where the first repeated member name was, if one was. */
    class Parsed(val root: SetupJson, val repeated: String?)

    /**
     * One JSON value, refusing an object that repeats a member name: parsers disagree about which
     * copy counts, so such a document is never merged (section 9.7 refuses it first).
     */
    fun parse(text: String): SetupJson {
        val parsed = Reader(text, refuseRepeats = true).document()
        return parsed.root
    }

    /**
     * One JSON value as `JSON.parse` reads it, which keeps the last copy of a repeated name where
     * the first one stood, together with the path of the first repeat in the order the text has
     * them: `machines[0].systems[1].agent`, the path section 9.7 reports `duplicate-member` at.
     */
    fun parseAllowingRepeats(text: String): Parsed = Reader(text, refuseRepeats = false).document()

    private class Reader(private val text: String, private val refuseRepeats: Boolean) {
        private var at = 0
        private var repeated: String? = null

        fun document(): Parsed {
            val root = value("")
            space()
            if (at != text.length) fail("text after the JSON value")
            return Parsed(root, repeated)
        }

        private fun fail(what: String): Nothing = throw Malformed("$what at offset $at")

        private fun space() {
            while (at < text.length && (text[at] == ' ' || text[at] == '\t' || text[at] == '\n' || text[at] == '\r')) at += 1
        }

        private fun value(path: String): SetupJson {
            space()
            if (at >= text.length) fail("a JSON value was expected")
            return when (val c = text[at]) {
                '{' -> objectValue(path)
                '[' -> arrayValue(path)
                '"' -> SetupJson.Text(string())
                't' -> literal("true", SetupJson.Bool(true))
                'f' -> literal("false", SetupJson.Bool(false))
                'n' -> literal("null", SetupJson.Null)
                else -> if (c == '-' || c in '0'..'9') number() else fail("a JSON value was expected")
            }
        }

        private fun objectValue(path: String): SetupJson.Object {
            at += 1
            val node = SetupJson.Object()
            space()
            if (at < text.length && text[at] == '}') {
                at += 1
                return node
            }
            while (true) {
                space()
                if (at >= text.length || text[at] != '"') fail("a member name was expected")
                val name = string()
                space()
                if (at >= text.length || text[at] != ':') fail("a colon was expected")
                at += 1
                val child = value(join(path, name))
                val index = node.members.indexOfFirst { it.first == name }
                if (index >= 0) {
                    if (refuseRepeats) fail("the member name ${SetupJson.quote(name)} is repeated")
                    if (repeated == null) repeated = join(path, name)
                    node.members[index] = name to child
                } else {
                    node.members += name to child
                }
                space()
                when {
                    at < text.length && text[at] == ',' -> at += 1
                    at < text.length && text[at] == '}' -> {
                        at += 1
                        return node
                    }

                    else -> fail("a comma or a closing brace was expected")
                }
            }
        }

        private fun arrayValue(path: String): SetupJson.Array {
            at += 1
            val node = SetupJson.Array()
            space()
            if (at < text.length && text[at] == ']') {
                at += 1
                return node
            }
            while (true) {
                node.items += value("$path[${node.items.size}]")
                space()
                when {
                    at < text.length && text[at] == ',' -> at += 1
                    at < text.length && text[at] == ']' -> {
                        at += 1
                        return node
                    }

                    else -> fail("a comma or a closing bracket was expected")
                }
            }
        }

        private fun string(): String {
            at += 1
            val out = StringBuilder()
            while (true) {
                if (at >= text.length) fail("an unterminated string")
                val c = text[at]
                when {
                    c == '"' -> {
                        at += 1
                        return out.toString()
                    }

                    c == '\\' -> {
                        if (at + 1 >= text.length) fail("an unterminated string")
                        when (val escape = text[at + 1]) {
                            '"' -> out.append('"')
                            '\\' -> out.append('\\')
                            '/' -> out.append('/')
                            'b' -> out.append('\b')
                            'f' -> out.append('\u000C')
                            'n' -> out.append('\n')
                            'r' -> out.append('\r')
                            't' -> out.append('\t')
                            'u' -> {
                                val hex = text.substring(at + 2, minOf(at + 6, text.length))
                                if (hex.length != 4 || hex.any { it !in '0'..'9' && it !in 'a'..'f' && it !in 'A'..'F' }) {
                                    fail("a \\u escape needs four hex digits")
                                }
                                out.append(hex.toInt(16).toChar())
                                at += 4
                            }

                            else -> fail("\\$escape is not an escape JSON has")
                        }
                        at += 2
                    }

                    c < ' ' -> fail("a control character inside a string")
                    else -> {
                        out.append(c)
                        at += 1
                    }
                }
            }
        }

        private fun number(): SetupJson.Number {
            val start = at
            if (text[at] == '-') at += 1
            if (at >= text.length || text[at] !in '0'..'9') fail("a JSON value was expected")
            if (text[at] == '0') at += 1 else digits()
            if (at < text.length && text[at] == '.') {
                at += 1
                if (at >= text.length || text[at] !in '0'..'9') fail("a digit was expected after the point")
                digits()
            }
            if (at < text.length && (text[at] == 'e' || text[at] == 'E')) {
                at += 1
                if (at < text.length && (text[at] == '+' || text[at] == '-')) at += 1
                if (at >= text.length || text[at] !in '0'..'9') fail("a digit was expected in the exponent")
                digits()
            }
            return SetupJson.Number(text.substring(start, at))
        }

        private fun digits() {
            while (at < text.length && text[at] in '0'..'9') at += 1
        }

        private fun literal(word: String, node: SetupJson): SetupJson {
            if (!text.startsWith(word, at)) fail("a JSON value was expected")
            at += word.length
            return node
        }
    }

    /** A member's path under [base], as section 9.7 writes problem paths. */
    fun join(base: String, key: String): String = if (base.isEmpty()) key else "$base.$key"
}
