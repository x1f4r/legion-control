using System.Numerics;
using System.Text.RegularExpressions;

namespace LegionControl.Desktop.Config;

/// When two JSON values are the same value (contract section 9.8).
///
/// Objects with the same names in any order and equal values, arrays element by element, strings by
/// code point, literals by identity, and numbers by their exact decimal value: `22`, `22.0` and
/// `2.2e1` are one number. No floating point decides it, because a double cannot tell `0.1` from
/// `0.10000000000000001`, and a merge that thinks two different values are equal keeps one of
/// them without asking. Absent equals only absent.
public static class JsonEquality
{
    private static readonly Regex NumberParts = new(
        @"\A(-?)(0|[1-9][0-9]*)(?:\.([0-9]+))?(?:[eE]([+-]?[0-9]+))?\z", RegexOptions.CultureInvariant);

    public static bool Equal(DocumentValue? a, DocumentValue? b)
    {
        if (a is null || b is null) return a is null && b is null;
        switch (a)
        {
            case DocumentObject left when b is DocumentObject right:
                if (left.Members.Count != right.Members.Count) return false;
                return left.Members.All(member => Equal(member.Value, right[member.Key]));
            case DocumentArray left when b is DocumentArray right:
                if (left.Items.Count != right.Items.Count) return false;
                for (var index = 0; index < left.Items.Count; index += 1)
                    if (!Equal(left.Items[index], right.Items[index])) return false;
                return true;
            case DocumentString left when b is DocumentString right:
                return string.Equals(left.Value, right.Value, StringComparison.Ordinal);
            case DocumentNumber left when b is DocumentNumber right:
                return DecimalKey(left.Lexeme) == DecimalKey(right.Lexeme);
            case DocumentLiteral left when b is DocumentLiteral right:
                return left.Value == right.Value;
            default:
                return false;
        }
    }

    /// A number lexeme as its exact decimal value, `<sign><digits>e<exponent>` with no leading or
    /// trailing zeros in the digits: `22`, `22.0`, `2.2e1` and `220e-1` are all `22e0`, and `0` and
    /// `-0` are both `0`. Two lexemes are the same number exactly when their keys are equal, at any
    /// size and precision.
    public static string DecimalKey(string lexeme)
    {
        var match = NumberParts.Match(lexeme);
        if (!match.Success) throw new FormatException($"\"{lexeme}\" is not a JSON number");
        var sign = match.Groups[1].Value;
        var whole = match.Groups[2].Value;
        var fraction = match.Groups[3].Value;
        var exponent = match.Groups[4].Success ? BigInteger.Parse(match.Groups[4].Value) : BigInteger.Zero;
        var digits = (whole + fraction).TrimStart('0');
        if (digits.Length == 0) return "0";
        var significant = digits.TrimEnd('0');
        var scale = exponent - fraction.Length + (digits.Length - significant.Length);
        return $"{sign}{significant}e{scale}";
    }

    /// The whole number a lexeme spells exactly, or null when it spells a fraction.
    public static BigInteger? Integer(string lexeme)
    {
        var key = DecimalKey(lexeme);
        if (key == "0") return BigInteger.Zero;
        var at = key.IndexOf('e');
        var scale = BigInteger.Parse(key[(at + 1)..]);
        if (scale < 0) return null;
        return BigInteger.Parse(key[..at]) * BigInteger.Pow(10, (int)scale);
    }
}
