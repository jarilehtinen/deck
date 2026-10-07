# Deck: adding a genre

You are helping the user add a genre to Deck, their terminal Spotify player. A genre in
Deck is a radio: Deck picks one of the genre's seeds (artists or tracks) at random and
plays that seed's Spotify radio station. When the station runs out, another seed takes
over. So the genre is only as good as its seeds: every seed's station should stay inside
the genre.

Your tools are `deck genre list`, `deck genre add`, a file you write for the candidates,
and web search.

## Steps

1. **See what exists.** Run `deck genre list`. It prints every genre with its seeds as
   JSON; the user's own genres have `"own": true`. If the genre is already there, say so
   and ask whether to replace it. Adding a genre with the same name replaces it: a
   built-in genre is replaced by the user's version, and an own genre gets the new
   seeds. The built-in genres are good examples of seeds.

2. **Pick 8–12 seeds.** See "What makes good seeds" below.

3. **Find the Spotify URIs.** Search the web for each seed, for example
   `The Troggs Spotify artist` or `She's Not There The Zombies Spotify track`, limited to
   open.spotify.com. The link `https://open.spotify.com/artist/57xdnSVt4ahJCIXYLieQ25`
   is the URI `spotify:artist:57xdnSVt4ahJCIXYLieQ25`, and `/track/…` is
   `spotify:track:…`. Search results often contain look-alikes ("Troggs", "The Trogss"):
   take the link whose title is exactly the artist or the track. Deck checks the name,
   so a wrong link is rejected, not saved.

   A seed without `uri` is looked up by name in the Spotify Web API. Spotify blocks the
   user's Deck for hours after too many searches, so Deck allows only 30 searches a day,
   shared with other Deck commands. Give a URI whenever you can.

4. **Check them with a dry run.** Write the genre as a JSON file and pass it to
   `deck genre add` on stdin:

   ```json
   {"name": "60s rock", "seeds": [
     {"artist": "The Troggs", "uri": "spotify:artist:57xdnSVt4ahJCIXYLieQ25"},
     {"artist": "The Zombies", "track": "She's Not There", "uri": "spotify:track:5BATmTqGopeifUzHN2bE0f"},
     {"artist": "Love"}
   ]}
   ```

   ```sh
   deck genre add --dry-run < genre.json
   ```

   `artist` is required, `track` makes the seed a track, and `uri` must match: an
   artist URI without `track`, a track URI with it. At most 20 seeds. Names are
   compared loosely: case, accents and punctuation do not matter, and a version
   suffix on Spotify ("Blue Suede Shoes - Original") is ignored.

   The report has:
   - `accepted`: the seeds that work, each with `station`: six tracks from its station
     (`Artist · Track (year)`). **Read them.** They show whether the station stays in
     the genre. Six tracks out of 50 vary from run to run, so judge the pattern, not a
     single track. The year is the release year of the album on Spotify, so a
     compilation or a remaster shows a later year than the song.
   - `rejected`: seeds that failed, with `reason`:
     - `invalid_uri`: not an artist or track URI, or the type does not match `track`.
     - `duplicate`: the same seed twice.
     - `not_found`: the URI does not exist, or the name search found nothing matching.
     - `name_mismatch`: the URI is somebody else; `spotify_name` tells who. Find the
       right link.
     - `no_station`: Spotify has no station for it. This is common: even big classic
       artists (Buddy Holly, Little Richard) can have no artist station. Try one of
       their typical tracks instead, or another seed.
     - `not_checked`: not searched, because the day's searches are used up. Give a URI.
   - `replaces`: `built-in` or `own` if the genre replaces an existing one.
   - `missing`: how many more accepted seeds are needed (at least 3).
   - `searched` and `searches_left`: Web API searches in this run and left today.

   Replace the rejected seeds, and the accepted ones whose station drifts out of the
   genre, then run the dry run again. Do at most three dry runs. A dry run can test
   extra candidates next to the ones you keep; dropping seeds afterwards needs no new
   dry run.

   If `deck genre add` fails with "Spotify is rate limiting Deck", stop searching by
   name: use only seeds with URIs, or stop and tell the user.

5. **Save the genre.** Remove the seeds you decided against from the file, then run the
   command without `--dry-run`. Deck checks the seeds again, saves the genre if at
   least 3 are accepted, and saves only the accepted ones. Tell the user to
   restart Deck: the genre then appears in Genres, where `a` puts it on the home view.

6. **Finish** with a short summary: the seeds, which parts of the genre they cover, and
   anything you were unsure about.

To remove an own genre: `deck genre remove <name>`. If it replaced a built-in genre, the
built-in version comes back.

## What makes good seeds

- **Different sides of the genre.** Cover its sub-styles, eras and scenes, for example
  lofi: jazzy (Nujabes), sleepy (Kupla, Sleepy Fish), chillhop (Philanthrope). Seeds
  from one circle give twelve stations that sound the same.
- **Not each other's neighbours.** Avoid seeds that are each other's closest related
  artists; their stations overlap.
- **Core, not the most famous.** Spotify's station leans towards the listener's own
  taste. A very popular artist's station spreads wide (Oasis or The Strokes become
  general alternative rock with the user's favourites mixed in). Central but narrower
  artists keep the station in the genre.
- **A track can be tighter than an artist.** If an artist's station spreads, a typical
  track of theirs may stay in the genre better (Blur · Parklife instead of Blur). Not
  always: check the sample.
- **Decades slip.** For a decade genre ("60s rock"), stations drift into the
  neighbouring decades, and remasters and compilations carry later years. Look at the
  years in the samples and replace seeds whose stations slip the most. Artists who were
  active only in that decade hold it best.
- **Facts.** Do not guess names. If you are unsure that an artist or track is on
  Spotify, search for it.
