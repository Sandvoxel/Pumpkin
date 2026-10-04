use std::{collections::BTreeMap, sync::Mutex};

use pumpkin_util::math::position::BlockPos;
use rustc_hash::FxHashSet;

use crate::tick::{OrderedTick, ScheduledTick};

pub struct ChunkTickScheduler<T> {
    inner: Mutex<ChunkTickSchedulerInner<T>>,
}

struct ChunkTickSchedulerInner<T> {
    current_tick: i64,
    tick_queue: BTreeMap<i64, Vec<OrderedTick<T>>>,
    queued_ticks: FxHashSet<(BlockPos, T)>,
}

impl<'a, T: std::hash::Hash + Eq> ChunkTickScheduler<&'a T> {
    pub fn step_tick(&self) -> Vec<OrderedTick<&'a T>> {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let current = inner.current_tick;
        inner.current_tick += 1;
        let mut result = Vec::new();
        while inner
            .tick_queue
            .first_key_value()
            .is_some_and(|(deadline, _)| *deadline <= current)
        {
            if let Some((_, ticks)) = inner.tick_queue.pop_first() {
                for tick in &ticks {
                    inner.queued_ticks.remove(&(tick.position, tick.value));
                }
                result.extend(ticks);
            }
        }
        result
    }

    pub fn schedule_tick(&self, tick: &ScheduledTick<&'a T>, sub_tick_order: i64) {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if inner.queued_ticks.insert((tick.position, tick.value)) {
            let deadline = inner.current_tick + i64::from(tick.delay);
            inner
                .tick_queue
                .entry(deadline)
                .or_default()
                .push(OrderedTick {
                    priority: tick.priority,
                    sub_tick_order,
                    position: tick.position,
                    value: tick.value,
                });
        }
    }

    pub fn is_scheduled(&self, pos: BlockPos, value: &T) -> bool {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .queued_ticks
            .contains(&(pos, value))
    }

    pub fn clear_area(&self, min: &BlockPos, max: &BlockPos) {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let contains = |position: &BlockPos| {
            position.0.x >= min.0.x
                && position.0.x < max.0.x
                && position.0.y >= min.0.y
                && position.0.y < max.0.y
                && position.0.z >= min.0.z
                && position.0.z < max.0.z
        };
        for queue in inner.tick_queue.values_mut() {
            queue.retain(|tick| !contains(&tick.position));
        }
        inner.tick_queue.retain(|_, ticks| !ticks.is_empty());
        inner
            .queued_ticks
            .retain(|(position, _)| !contains(position));
    }

    pub fn has_ticks(&self) -> bool {
        !self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .queued_ticks
            .is_empty()
    }

    #[must_use]
    pub fn to_vec(&self) -> Vec<ScheduledTick<&'a T>> {
        let inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // LevelChunkTicks.pack stores insertion order so unpack can restore ties.
        let mut ticks: Vec<_> = inner
            .tick_queue
            .iter()
            .flat_map(|(&deadline, ticks)| ticks.iter().map(move |tick| (deadline, tick)))
            .collect();
        ticks.sort_by_key(|(_, tick)| tick.sub_tick_order);
        ticks
            .into_iter()
            .map(|(deadline, tick)| ScheduledTick {
                delay: (deadline - inner.current_tick) as i32,
                priority: tick.priority,
                position: tick.position,
                value: tick.value,
            })
            .collect()
    }
}

impl<'a, T: std::hash::Hash + Eq + 'static> FromIterator<ScheduledTick<&'a T>>
    for ChunkTickScheduler<&'a T>
{
    fn from_iter<I: IntoIterator<Item = ScheduledTick<&'a T>>>(iter: I) -> Self {
        let scheduler = Self::default();
        let ticks: Vec<_> = iter.into_iter().collect();
        let base = -(ticks.len() as i64);
        for (index, tick) in ticks.iter().enumerate() {
            scheduler.schedule_tick(tick, base + index as i64);
        }
        scheduler
    }
}

impl<T> Default for ChunkTickScheduler<T> {
    fn default() -> Self {
        Self {
            inner: Mutex::new(ChunkTickSchedulerInner {
                current_tick: 0,
                tick_queue: BTreeMap::new(),
                queued_ticks: FxHashSet::default(),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tick::TickPriority;
    use pumpkin_data::Block;

    #[test]
    fn long_signed_delays_and_ties_survive_reload() {
        let tick = |x, delay, priority| ScheduledTick {
            delay,
            priority,
            position: BlockPos::new(x, 64, 0),
            value: &Block::STONE,
        };
        let scheduler = ChunkTickScheduler::default();
        scheduler.schedule_tick(&tick(0, 70_000, TickPriority::Low), 0);
        scheduler.schedule_tick(&tick(1, 258, TickPriority::Normal), 1);
        scheduler.schedule_tick(&tick(2, 258, TickPriority::Normal), 2);
        scheduler.schedule_tick(&tick(3, -7, TickPriority::High), 3);
        assert_eq!(scheduler.to_vec()[3].delay, -7);
        assert_eq!(scheduler.step_tick()[0].position, BlockPos::new(3, 64, 0));
        for _ in 1..256 {
            assert!(scheduler.step_tick().is_empty());
        }
        let saved = scheduler.to_vec();
        assert_eq!(saved[0].delay, 69_744);
        let reloaded: ChunkTickScheduler<_> = saved.into_iter().collect();
        assert!(reloaded.step_tick().is_empty());
        assert!(reloaded.step_tick().is_empty());
        let mut due = reloaded.step_tick();
        due.sort_unstable();
        assert_eq!(
            due.iter().map(|tick| tick.position.0.x).collect::<Vec<_>>(),
            vec![1, 2]
        );
        assert_eq!(reloaded.to_vec()[0].delay, 69_741);
    }
}
