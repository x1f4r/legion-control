namespace LegionControl.Desktop.Updates;

/// Keeps a verified update visible while discovery is throttled or temporarily unavailable.
public sealed class AppUpdateSuggestions(
    Func<string, CancellationToken, Task<UpdateAvailability>> check,
    Func<DateTimeOffset>? clock = null)
{
    public static readonly TimeSpan ForegroundInterval = TimeSpan.FromMinutes(15);
    public static readonly TimeSpan PeriodicInterval = TimeSpan.FromHours(6);
    private readonly object _gate = new();
    private readonly Func<DateTimeOffset> _clock = clock ?? (() => DateTimeOffset.UtcNow);
    private string? _repo;
    private long _generation;
    private DateTimeOffset? _lastAttempt;
    private Task<UpdateAvailability>? _inFlight;
    public UpdateAvailability? Availability { get; private set; }
    public UpdateAvailability? LastResult { get; private set; }
    public event Action? Changed;

    public void SelectRepository(string repo)
    {
        bool changed;
        lock (_gate) changed = SelectRepositoryCore(repo);
        if (changed) Changed?.Invoke();
    }

    private bool SelectRepositoryCore(string repo)
    {
        if (_repo == repo) return false;
        _repo = repo;
        _generation++;
        _lastAttempt = null;
        _inFlight = null;
        Availability = null;
        LastResult = null;
        return true;
    }

    public AppUpdateReview? BeginReview(string repo, UpdateAvailability.Ready ready)
    {
        lock (_gate)
            return _repo == repo && ReferenceEquals(Availability, ready)
                ? new AppUpdateReview(repo, _generation, ready) : null;
    }

    public bool IsCurrent(AppUpdateReview review, string repo)
    {
        lock (_gate) return review.Repository == repo && _repo == repo && review.Generation == _generation;
    }

    public Task<UpdateAvailability> CheckAsync(string repo, bool force = false, CancellationToken cancellationToken = default)
    {
        TaskCompletionSource<UpdateAvailability> completion;
        lock (_gate)
        {
            // A shared setup can change the release repository while a request is running.
            // Its answer must never replace an offer from the new repository.
            SelectRepositoryCore(repo);
            if (_inFlight is not null) return _inFlight;
            if (!force && _lastAttempt is { } last && _clock() - last < ForegroundInterval && LastResult is { } result)
                return Task.FromResult(result);
            _lastAttempt = _clock();
            completion = new(TaskCreationOptions.RunContinuationsAsynchronously);
            _inFlight = completion.Task;
        }
        Changed?.Invoke();
        _ = CompleteAsync(repo, completion, cancellationToken);
        return completion.Task;
    }

    private async Task CompleteAsync(string repo, TaskCompletionSource<UpdateAvailability> completion, CancellationToken cancellationToken)
    {
        UpdateAvailability result;
        try
        {
            using var budget = CancellationTokenSource.CreateLinkedTokenSource(cancellationToken);
            budget.CancelAfter(TimeSpan.FromSeconds(60));
            result = await check(repo, budget.Token).ConfigureAwait(false);
        }
        catch (Exception error)
        {
            result = new UpdateAvailability.Unavailable(error is OperationCanceledException
                ? "The app update check was interrupted. It will be tried again."
                : $"The app update could not be checked: {error.Message}");
        }
        lock (_gate)
        {
            if (_repo == repo && ReferenceEquals(_inFlight, completion.Task))
            {
                LastResult = result;
                if (result is not UpdateAvailability.Unavailable || Availability is not UpdateAvailability.Ready)
                    Availability = result;
                _inFlight = null;
            }
        }
        completion.TrySetResult(result);
        Changed?.Invoke();
    }
}

public sealed record AppUpdateReview(string Repository, long Generation, UpdateAvailability.Ready Ready);
