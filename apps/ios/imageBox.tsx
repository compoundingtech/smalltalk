// An image in a conversation, a message or an ask, drawn the same way everywhere: a box of one size
// that says the image's name, type and real size, reads its bytes only while it is on screen, says
// in words that it is loading or why it cannot be shown, and opens on a tap to zoom (pinch, Close)
// and to share or save through the person's own apps. The words come from imageView.ts.
import { Component, createRef, useEffect, useMemo, useState } from 'react';
import { Image, Modal, Pressable, ScrollView, Share, StyleSheet, useWindowDimensions, View } from 'react-native';
import { useSafeAreaInsets } from 'react-native-safe-area-context';
import { imageLabel, imageWords, type ImageFacts } from './imageView';
import { ConversationImageViewport, VisibleConversationImage, type VisibleImageState } from './visibleConversationImages';
import type { MailImage } from '@smalltalk/st3-views';
import { useStore } from './store';
import { theme } from './theme';
import { T } from './ui';

type Props = { facts: ImageFacts; viewport: ConversationImageViewport; load: (current: () => boolean) => Promise<string> };

// Class lifecycle owns native measurement and invalidates late responses on unmount.
// All mounted images share the viewport queue; FlatList overscan never authorizes reads.
export class ImageBox extends Component<Props, VisibleImageState<string> & { zoomed?: boolean }> {
  state: VisibleImageState<string> & { zoomed?: boolean } = { kind: 'closed' };
  private box = createRef<View>();
  private unregister?: () => void;
  private image = new VisibleConversationImage<string>(this.props.viewport, current => this.props.load(current), state => this.setState(state.kind === 'image' ? state : { ...state, zoomed: false }));

  componentDidMount() {
    this.unregister = this.props.viewport.register({
      measure: done => this.box.current?.measureInWindow((x, y, width, height) => done({ x, y, width, height })),
      visible: shown => this.image.setVisible(shown),
    });
  }

  componentWillUnmount() { this.image.dispose(); this.unregister?.(); }

  render() {
    const state = this.state;
    const words = imageWords(state, this.props.facts);
    const label = imageLabel(this.props.facts);
    return <View style={{ gap: 2 }}>
      <Pressable ref={this.box} collapsable={false} style={styles.box} onLayout={() => this.props.viewport.refresh()}
        accessibilityRole={state.kind === 'image' ? 'imagebutton' : undefined} accessibilityLabel={state.kind === 'image' ? `${label}, open` : words.text}
        disabled={state.kind !== 'image'} onPress={() => this.setState({ zoomed: true })}>
        {state.kind === 'image'
          // React Native's iOS loader decodes the image at this box's size, not the file's.
          ? <Image source={{ uri: state.image }} style={StyleSheet.absoluteFill} resizeMode="contain" onError={() => this.image.fail('The image could not be displayed. Try again.')} />
          : <T dim={words.tone === 'dim'} color={words.tone === 'fault' ? theme.red : undefined} style={{ fontSize: 11, padding: 6 }}>🖼 {words.text}</T>}
      </Pressable>
      {state.kind === 'image' ? <T dim style={{ fontSize: 11 }}>{label}  <T color={theme.accent} style={{ fontSize: 11 }}>tap to open</T></T> : null}
      {words.retry ? <Pressable accessibilityRole="button" onPress={() => this.image.retry()}><T color={theme.accent}>Retry loading image</T></Pressable> : null}
      {state.kind === 'image' && state.zoomed ? <ImageZoom uri={state.image} label={label} onClose={() => this.setState({ zoomed: false })} /> : null}
    </View>;
  }
}

/** The image on the whole screen: pinch to zoom, Close, and Share to open or save it elsewhere. */
export function ImageZoom({ uri, label, onClose }: { uri: string; label: string; onClose: () => void }) {
  const { width, height } = useWindowDimensions();
  const insets = useSafeAreaInsets();
  const [shareFailed, setShareFailed] = useState('');
  return <Modal visible animationType="fade" presentationStyle="fullScreen" onRequestClose={onClose}>
    <View style={{ flex: 1, backgroundColor: theme.crust }}>
      <View style={[styles.bar, { paddingTop: insets.top + 6 }]}>
        <Pressable accessibilityRole="button" onPress={onClose} hitSlop={10}><T bold color={theme.accent}>Close</T></Pressable>
        <T dim numberOfLines={1} style={{ flex: 1, textAlign: 'center' }}>{label}</T>
        {/* The share sheet offers Save Image and the person's own apps; nothing is saved here. */}
        <Pressable accessibilityRole="button" hitSlop={10} onPress={() => void Share.share({ url: uri }).catch(error => setShareFailed(error instanceof Error ? error.message : String(error)))}><T bold color={theme.accent}>Share</T></Pressable>
      </View>
      {shareFailed ? <T color={theme.red} style={{ padding: 8 }}>Could not share: {shareFailed}</T> : null}
      <ScrollView style={{ flex: 1 }} contentContainerStyle={{ flexGrow: 1, alignItems: 'center', justifyContent: 'center' }}
        maximumZoomScale={8} minimumZoomScale={1} bouncesZoom centerContent showsHorizontalScrollIndicator={false} showsVerticalScrollIndicator={false}>
        <Image accessibilityLabel={label} source={{ uri }} style={{ width, height: height - insets.top - insets.bottom - 48 }} resizeMode="contain" />
      </ScrollView>
    </View>
  </Modal>;
}

// An image a message carries, read from st (from the owner of the message, through st) only while
// it is on screen; its box keeps one size while it loads, so the conversation does not move.
export function MailImageView({ image, viewport }: { image: MailImage; viewport: ConversationImageViewport }) {
  const { actions } = useStore();
  return <ImageBox facts={{ name: image.name, mediaType: image.mediaType, size: image.size }} viewport={viewport} load={() => actions.image(image)} />;
}

/** A viewport the size of the window, for images outside a conversation's list (alerts, asks). */
export function useWindowViewport(): ConversationImageViewport {
  const viewport = useMemo(() => new ConversationImageViewport(), []);
  const { width, height } = useWindowDimensions();
  useEffect(() => { viewport.setBounds({ x: 0, y: 0, width, height }); }, [viewport, width, height]);
  useEffect(() => { viewport.setActive(true); return () => viewport.setActive(false); }, [viewport]);
  return viewport;
}

const styles = StyleSheet.create({
  box: { width: 200, height: 150, borderRadius: 6, overflow: 'hidden', backgroundColor: theme.surface0 },
  bar: { flexDirection: 'row', alignItems: 'center', gap: 12, paddingHorizontal: 14, paddingBottom: 8, backgroundColor: theme.mantle },
});
