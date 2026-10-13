//! The decoded images a screen holds.
//!
//! The main and the alternate screen each have a store: the protocol ties an
//! image to the screen it was sent to, an alternate-screen program's images
//! go when it leaves, and the main screen's wait for it to come back.

use std::sync::Arc;

use super::command::{Command, Scope};
use super::receive::Decoded;
use super::{Echo, Error, Reply};

/// Ids the terminal hands out for an image sent by number (`I=`) start
/// here, far from the small ids programs choose themselves.
const AUTO_ID_BASE: u32 = 0x8000_0000;

/// One stored image.
pub(crate) struct Image {
    /// The id programs address it by; 0 for an anonymous one.
    pub(crate) id: u32,
    /// The number it was sent with (`I=`); 0 for none.
    pub(crate) number: u32,
    #[allow(dead_code, reason = "read by the frame sink that draws the image")]
    pub(crate) width: u32,
    #[allow(dead_code, reason = "read by the frame sink that draws the image")]
    pub(crate) height: u32,
    /// RGBA, shared with whoever draws it.
    #[allow(dead_code, reason = "read by the frame sink that draws the image")]
    pub(crate) pixels: Arc<Vec<u8>>,
    /// Counts the images stored on this session, so a re-sent id is told
    /// apart from the image it replaced.
    #[allow(dead_code, reason = "read by the frame sink that draws the image")]
    pub(crate) generation: u64,
    /// How many placements show it.
    placements: u32,
}

impl Image {
    fn bytes(&self) -> usize {
        self.pixels.len()
    }
}

/// One screen's images, oldest first.
pub(crate) struct Store {
    images: Vec<Image>,
    bytes: usize,
    quota: usize,
    next_auto: u32,
}

impl Store {
    fn new(quota: usize) -> Self {
        Self {
            images: Vec::new(),
            bytes: 0,
            quota,
            next_auto: AUTO_ID_BASE,
        }
    }

    fn has(&self, id: u32) -> bool {
        self.images.iter().any(|image| image.id == id)
    }

    fn fresh_id(&mut self) -> u32 {
        loop {
            let id = self.next_auto;
            self.next_auto = id.checked_add(1).unwrap_or(AUTO_ID_BASE);
            if !self.has(id) {
                return id;
            }
        }
    }

    fn remove(&mut self, index: usize) {
        self.bytes -= self.images.remove(index).bytes();
    }

    /// Keeps `image`, replacing a stored image of the same id, and makes
    /// room: images no placement shows go first, oldest first, then the
    /// oldest of the rest. The new image is never the one that goes.
    fn insert(&mut self, image: Image) -> Result<(), Error> {
        let size = image.bytes();
        if size > self.quota {
            return Err(Error::no_space("the image does not fit the storage quota"));
        }
        if image.id != 0
            && let Some(old) = self.images.iter().position(|kept| kept.id == image.id)
        {
            self.remove(old);
        }
        self.bytes += size;
        self.images.push(image);
        while self.bytes > self.quota {
            let newest = self.images.len() - 1;
            let victim = self
                .images
                .iter()
                .take(newest)
                .position(|image| image.placements == 0)
                .unwrap_or(0);
            self.remove(victim);
        }
        Ok(())
    }

    /// Removes what a delete command names. A delete ends the placements it
    /// matches; the image goes with its last placement when the command is
    /// in upper case, and an anonymous image (nothing can address it again)
    /// goes either way. Images the command does not name stay.
    fn delete(&mut self, command: &Command) {
        let named = |image: &Image| match command.scope {
            Scope::Visible => true,
            Scope::Id => image.id != 0 && image.id == command.echo.id,
            Scope::Range => image.id != 0 && (command.x..=command.y).contains(&image.id),
            // Handled below: only the newest image of a number.
            Scope::Number => false,
            // Placements by where they are: none exist to match.
            Scope::Placements => false,
        };
        let mut index = self.images.len();
        let newest_numbered = (command.scope == Scope::Number && command.echo.number != 0)
            .then(|| {
                self.images
                    .iter()
                    .rposition(|image| image.number == command.echo.number)
            })
            .flatten();
        while index > 0 {
            index -= 1;
            let image = &self.images[index];
            let hit = named(image) || newest_numbered == Some(index);
            if hit && image.placements == 0 && (command.free || image.id == 0) {
                self.remove(index);
            }
        }
    }

    fn clear(&mut self) {
        self.images.clear();
        self.bytes = 0;
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.images.len()
    }

    #[cfg(test)]
    pub(crate) fn bytes(&self) -> usize {
        self.bytes
    }

    #[cfg(test)]
    pub(crate) fn get(&self, id: u32) -> Option<&Image> {
        self.images.iter().find(|image| image.id == id)
    }

    #[cfg(test)]
    pub(crate) fn ids(&self) -> Vec<u32> {
        self.images.iter().map(|image| image.id).collect()
    }

    #[cfg(test)]
    pub(crate) fn set_placements(&mut self, id: u32, placements: u32) {
        for image in self.images.iter_mut().filter(|image| image.id == id) {
            image.placements = placements;
        }
    }
}

/// Both screens' stores.
pub(crate) struct Images {
    main: Store,
    alt: Store,
    generation: u64,
}

impl Images {
    pub(crate) fn with_quota(quota: usize) -> Self {
        Self {
            main: Store::new(quota),
            alt: Store::new(quota),
            generation: 0,
        }
    }

    fn screen(&mut self, alt: bool) -> &mut Store {
        if alt { &mut self.alt } else { &mut self.main }
    }

    /// Stores a transmitted image on a screen and answers the command. An
    /// image sent by number gets an id of the terminal's choosing, and the
    /// answer says which.
    pub(crate) fn store(&mut self, alt: bool, command: &Command, decoded: Decoded) -> Reply {
        self.generation += 1;
        let generation = self.generation;
        let store = self.screen(alt);
        let mut echo: Echo = command.echo;
        if echo.id == 0 && echo.number != 0 {
            echo.id = store.fresh_id();
        }
        let image = Image {
            id: echo.id,
            number: echo.number,
            width: decoded.width,
            height: decoded.height,
            pixels: Arc::new(decoded.pixels),
            generation,
            placements: 0,
        };
        match store.insert(image) {
            Ok(()) => Reply::ok(echo),
            Err(error) => Reply::failed(echo, error),
        }
    }

    pub(crate) fn delete(&mut self, alt: bool, command: &Command) {
        self.screen(alt).delete(command);
    }

    /// The alternate screen was entered or left.
    pub(crate) fn drop_alt(&mut self) {
        self.alt.clear();
    }

    #[cfg(test)]
    pub(crate) fn main(&self) -> &Store {
        &self.main
    }

    #[cfg(test)]
    pub(crate) fn alt(&self) -> &Store {
        &self.alt
    }

    #[cfg(test)]
    pub(crate) fn main_mut(&mut self) -> &mut Store {
        &mut self.main
    }
}
