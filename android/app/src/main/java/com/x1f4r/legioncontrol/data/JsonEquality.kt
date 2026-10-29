package com.x1f4r.legioncontrol.data

import java.math.BigInteger

/**
 * When two JSON values are the same value (contract section 9.8).
 *
 * Objects with the same names in any order and equal values, arrays element by element, strings by
 * code point, literals by identity, and numbers by their exact decimal value: `22`, `22.0`, `2.2e1`
 * and `220e-1` are one number, and `0.1` and `0.10000000000000001` are two, even though a double
 * cannot tell them apart. No floating point decides anything here, which is what lets a merge say
 * "nothing changed" about a value another client merely spelled differently, and still notice a
 * change no double would show.
 *
 * A port of `jsonEqual` and `decimalKey` in `contract/tools/setup-sync.mjs`, pinned by the
 * `numbers` group of `contract/vectors/setup-merge.json`.
 */
object JsonEquality {

    private val NUMBER = Regex("^(-?)(0|[1-9][0-9]*)(?:\\.([0-9]+))?(?:[eE]([+-]?[0-9]+))?$")

    /**
     * A number lexeme as its exact decimal value, `<sign><digits>e<exponent>` with no leading or
     * trailing zeros in the digits. Two lexemes are the same number exactly when their keys are
     * equal, at any size and precision. Zero is `0`, whatever its sign.
     */
    fun decimalKey(lexeme: String): String {
        val match = NUMBER.matchEntire(lexeme) ?: throw IllegalArgumentException("\"$lexeme\" is not a JSON number")
        val sign = match.groupValues[1]
        val whole = match.groupValues[2]
        val fraction = match.groupValues[3]
        val exponent = match.groupValues[4].ifEmpty { "0" }
        val digits = (whole + fraction).trimStart('0')
        if (digits.isEmpty()) return "0"
        val significant = digits.trimEnd('0')
        val scale = BigInteger(exponent.removePrefix("+")) -
            BigInteger.valueOf(fraction.length.toLong()) +
            BigInteger.valueOf((digits.length - significant.length).toLong())
        return "$sign${significant}e$scale"
    }

    /** Whether two lexemes are the same number. */
    fun sameNumber(a: String, b: String): Boolean = decimalKey(a) == decimalKey(b)

    /** The integer a number holds, or null when it holds none: `20`, `2e1` and `20.0` all hold 20. */
    fun integerOf(node: SetupJson?): BigInteger? {
        if (node !is SetupJson.Number) return null
        val key = decimalKey(node.lexeme)
        if (key == "0") return BigInteger.ZERO
        val digits = key.substringBefore('e')
        val scale = BigInteger(key.substringAfter('e'))
        // A whole number past the range of a double is not one any document can hold: section 9.7
        // reads numbers as JavaScript does, and such a revision is not an integer there.
        if (scale.signum() < 0 || scale.bitLength() > 16) return null
        return BigInteger(digits) * BigInteger.TEN.pow(scale.toInt())
    }

    /** Whether two values are the same JSON value. Absent (null) equals only absent. */
    fun equal(a: SetupJson?, b: SetupJson?): Boolean {
        if (a == null || b == null) return a == null && b == null
        return when (a) {
            is SetupJson.Object -> b is SetupJson.Object && a.members.size == b.members.size &&
                a.members.all { (name, value) -> equal(value, b[name]) }

            is SetupJson.Array -> b is SetupJson.Array && a.items.size == b.items.size &&
                a.items.indices.all { equal(a.items[it], b.items[it]) }

            is SetupJson.Text -> b is SetupJson.Text && a.value == b.value
            is SetupJson.Number -> b is SetupJson.Number && sameNumber(a.lexeme, b.lexeme)
            is SetupJson.Bool -> b is SetupJson.Bool && a.value == b.value
            SetupJson.Null -> b == SetupJson.Null
        }
    }
}
