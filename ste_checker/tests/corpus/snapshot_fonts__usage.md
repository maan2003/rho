```rust
use snapshot_fonts::{SnapshotFillLevels, SnapshotCandles};

// Price sparkline
let chart = SnapshotFillLevels::from_prices(&prices).draw();

// Candlestick chart
let chart = SnapshotCandles::from_prices(&prices).draw();
```
