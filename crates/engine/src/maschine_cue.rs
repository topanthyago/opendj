//! Playback-only Cue routing for the embedded SDK.
use deckard_model::Project;

/// Isolate the SDK Cue bus in the playback copy. The saved document retains all
/// user routing. An existing distinct Cue output also carries its shared routes.
pub fn playback_project(project: &Project) -> Result<(Project, usize), String> {
    use deckard_model::AudioDest;
    let mut playback = project.clone();
    let existing = if let AudioDest::Ext(bus) = project.cue_output {
        if (0..deckard_engine::OUTPUTS as i64).contains(&bus)
            && project.cue_output != project.master_output
        {
            Some(bus as usize)
        } else {
            None
        }
    } else {
        None
    };
    let mut used = [false; deckard_engine::OUTPUTS];
    let mut mark = |dest: AudioDest| {
        if let AudioDest::Ext(bus) = dest {
            if let Some(entry) = usize::try_from(bus).ok().and_then(|bus| used.get_mut(bus)) {
                *entry = true;
            }
        }
    };
    mark(project.master_output);
    for group in &project.groups {
        mark(group.channel.dest);
        for dest in group.channel.aux_dest {
            mark(dest);
        }
        for sound in &group.sounds {
            mark(sound.channel.dest);
            for dest in sound.channel.aux_dest {
                mark(dest);
            }
        }
    }
    for output in &project.extra_outs {
        mark(output.dest);
    }
    // An SDK project can occupy all 16 buses. An out-of-range Cue destination
    // is silent, preserving the other groups until an output becomes available.
    let cue_bus = existing
        .or_else(|| used.iter().position(|used| !used))
        .unwrap_or(deckard_engine::OUTPUTS);
    let cue_dest = AudioDest::Ext(cue_bus as i64);
    playback.cue_output = cue_dest;
    for (g, group) in playback
        .groups
        .iter_mut()
        .enumerate()
        .filter(|(_, group)| group.cue)
    {
        for destination in &mut group.channel.aux_dest {
            if *destination != AudioDest::None {
                *destination = cue_dest;
            }
        }
        for sound in &mut group.sounds {
            if !matches!(sound.channel.dest, AudioDest::None | AudioDest::Parent)
                && sound.channel.dest != AudioDest::Group(g)
                && !matches!(sound.channel.dest, AudioDest::Sound(group, _) if group == g)
            {
                sound.channel.dest = cue_dest;
            }
            for destination in &mut sound.channel.aux_dest {
                if *destination != AudioDest::None {
                    *destination = cue_dest;
                }
            }
        }
    }
    for output in &mut playback.extra_outs {
        use deckard_model::extra_out::Owner;
        let group = match output.owner {
            Owner::Sound(group, _) | Owner::Chain(group, _, _) | Owner::Group(group, _) => {
                Some(group)
            }
            Owner::Master(_) => None,
        };
        if group.is_some_and(|group| playback.groups.get(group).is_some_and(|group| group.cue)) {
            output.dest = cue_dest;
        }
    }
    Ok((playback, cue_bus))
}
