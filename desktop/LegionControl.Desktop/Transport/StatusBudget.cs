namespace LegionControl.Desktop.Transport;

/// The `--budget-ms` a status attempt sends, or that no attempt should be made (section 15.5,
/// step 3).
///
/// Five seconds stay in hand for ssh and the reply, the agent never gets more than 20 s, and it
/// never gets less than 1 s: a first attempt always goes out, a later one only when there is time
/// to use its answer. Sending 20000 whatever was left meant that after two dead routes the working
/// one was cut off by this app's own deadline before the agent's budget ran out.
public static class StatusBudget
{
    public readonly record struct Decision(bool Attempt, int? BudgetMs);

    public static Decision Next(long remainingMs, int attemptsMade)
    {
        var budget = Math.Min(20000, remainingMs - 5000);
        if (budget >= 1000) return new Decision(true, (int)budget);
        return attemptsMade == 0 ? new Decision(true, 1000) : new Decision(false, null);
    }
}
